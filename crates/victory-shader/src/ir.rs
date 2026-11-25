//! One-to-one SSA form of a native program (decision 0003, step 1).
//!
//! Every machine instruction becomes one [`Inst`], except scheduling-only
//! ones (`s_nop`, `s_waitcnt*`, `s_inst_prefetch`...) and redundant conditional
//! branches.
//! Each register dword is a variable: a read names the [`Value`] it sees, a
//! write defines a new one, and [`Phi`]s merge values where control flow
//! joins. Nothing is raised: an instruction keeps its opcode and decoded
//! fields, so its exact semantics stay with it.
//!
//! Implicit operands are explicit: SCC, M0, and EXEC, which every vector
//! instruction reads. A write to a VGPR also reads the register's old value,
//! because lanes disabled in EXEC keep it.
//!
//! M0-relative VGPR access (`v_movrels/d/sd_b32`, `v_swaprel_b32`) names its
//! operand's whole possible range: from the encoded register up to the
//! program's VGPR count or the largest M0 proven there by [`build_bounded`],
//! all read (and all written, for a destination).

use std::collections::HashMap;

use crate::ShaderError;
use crate::gfx10::{self, Fields, Flow, Instruction, Operand};

/// One register dword. Scalar codes as in [`Operand::Scalar`]: 0-105 SGPRs,
/// 106/107 VCC, 108-123 trap temporaries, 124 M0, 126/127 EXEC. Codes from
/// [`PARKED`] up are scalars the compiler parked in VGPR lanes (`unspill`):
/// no machine instruction names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reg {
    S(u8),
    Scc,
    V(u8),
}

pub const VCC: u8 = 106;
pub const M0: u8 = 124;
pub const EXEC: u8 = 126;
/// The first scalar code past the machine's (0-127): see [`Reg`].
pub const PARKED: u8 = 128;

/// An SSA value: the content of one register dword (SCC: one bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub wave64: bool,
    /// VGPRs v0 up to this exist; bounds M0-relative access.
    pub vgprs: u16,
    /// In code order, then copies made to give every loop one entry; block 0
    /// is the entry.
    pub blocks: Vec<Block>,
    /// Registers read before any write, with their value on entry: what the
    /// hardware or a called fetch routine set up (user SGPRs, system values,
    /// vertex inputs).
    pub inputs: Vec<(Reg, Value)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Byte offset of the block's first machine instruction (shared by a
    /// block and its copies).
    pub offset: usize,
    pub preds: Vec<usize>,
    pub phis: Vec<Phi>,
    pub insts: Vec<Inst>,
    pub exit: Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Jump(usize),
    /// `s_cbranch_*`, the block's last instruction.
    Branch {
        taken: usize,
        not_taken: usize,
    },
    /// `s_endpgm` and its variants.
    End,
    /// `s_setpc_b64` to an address held in registers, e.g. the handover to
    /// the next half of a merged hardware stage.
    Indirect,
}

impl Exit {
    pub fn successors(&self) -> Vec<usize> {
        match *self {
            Exit::Jump(b) => vec![b],
            Exit::Branch { taken, not_taken } => vec![taken, not_taken],
            Exit::End | Exit::Indirect => vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phi {
    pub reg: Reg,
    pub value: Value,
    /// One per predecessor, in [`Block::preds`] order.
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inst {
    pub machine: Instruction,
    pub fields: Fields,
    /// Every register dword read, with the value it holds.
    pub reads: Vec<(Reg, Value)>,
    /// Every register dword written, with the value it receives.
    pub writes: Vec<(Reg, Value)>,
}

impl Inst {
    pub fn name(&self) -> &'static str {
        self.machine.name().unwrap_or("")
    }

    /// The value `reg` holds when this instruction reads it.
    pub fn read(&self, reg: Reg) -> Option<Value> {
        self.reads.iter().find(|r| r.0 == reg).map(|r| r.1)
    }

    /// The value this instruction writes to `reg`.
    pub fn write(&self, reg: Reg) -> Option<Value> {
        self.writes.iter().find(|w| w.0 == reg).map(|w| w.1)
    }
}

/// Decodes `code` and builds its SSA form; `vgprs` is the program's VGPR
/// count. Fails on instructions the IR cannot express yet
/// ([`ShaderError::Unsupported`]).
pub fn build(code: &[u8], wave64: bool, vgprs: u16) -> Result<Program, ShaderError> {
    build_bounded(code, wave64, vgprs, &HashMap::new())
}

/// [`build`], with the largest M0 each relative move can see by instruction
/// offset, bounding the registers it names.
pub fn build_bounded(
    code: &[u8],
    wave64: bool,
    vgprs: u16,
    m0_max: &HashMap<usize, u8>,
) -> Result<Program, ShaderError> {
    let mut list = gfx10::decode(code)?;
    let lanes = 1 + u8::from(wave64);
    let isa = |offset, reason| ShaderError::Isa { offset, reason };

    let mut flows = Vec::with_capacity(list.len());
    let mut leaders = vec![0];
    for (i, inst) in list.iter().enumerate() {
        let flow = gfx10::control(inst, code.len(), |at| {
            list[..list.partition_point(|i| i.offset < at)].last()
        })?;
        if let Flow::Jump(t) | Flow::Branch(t) = flow {
            leaders.push(t);
        }
        if flow != Flow::Next
            && let Some(next) = list.get(i + 1)
        {
            leaders.push(next.offset);
        }
        flows.push(flow);
    }
    leaders.sort_unstable();
    leaders.dedup();
    let parked = unspill(&mut list, &leaders, &flows, wave64, lanes, vgprs);
    let block_at = |offset: usize| {
        leaders
            .binary_search(&offset)
            .map_err(|_| isa(offset, "jump into a block"))
    };

    let mut blocks: Vec<Block> = leaders
        .iter()
        .map(|&offset| Block {
            offset,
            preds: Vec::new(),
            phis: Vec::new(),
            insts: Vec::new(),
            exit: Exit::End,
        })
        .collect();
    let next_offsets: Vec<Option<usize>> = (0..list.len())
        .map(|i| list.get(i + 1).map(|n| n.offset))
        .collect();
    for (n, ((machine, flow), next)) in list.into_iter().zip(flows).zip(next_offsets).enumerate() {
        let b = leaders.partition_point(|&l| l <= machine.offset) - 1;
        let name = machine.name().ok_or(ShaderError::Unsupported {
            offset: machine.offset,
            what: "unknown opcode",
        })?;
        let mut fields = machine.fields(wave64);
        // A move to or from a scalar parked in a VGPR lane (`unspill`): its register
        // codes may be ones the machine word cannot hold.
        if let Ok(k) = parked.binary_search_by_key(&n, |m| m.0)
            && let Fields::Sop { dst: Some(Operand::Scalar { code: to, .. }), src, .. } = &mut fields
            && let Some(Operand::Scalar { code: from, .. }) = src.first_mut()
        {
            (*to, *from) = (parked[k].1, parked[k].2);
        }
        let fallthrough = || block_at(next.ok_or(isa(machine.offset, "falls off the code"))?);
        // A conditional SOPP branch to fallthrough contributes one edge to
        // the join. Calls also use Flow::Branch but write a return address.
        if machine.encoding == gfx10::Encoding::Sopp
            && matches!(flow, Flow::Branch(target) if Some(target) == next)
        {
            blocks[b].exit = Exit::Jump(fallthrough()?);
            continue;
        }
        let ends = flow != Flow::Next || next.is_none_or(|n| leaders.binary_search(&n).is_ok());
        if ends {
            blocks[b].exit = match flow {
                Flow::Next => Exit::Jump(fallthrough()?),
                Flow::Jump(t) => Exit::Jump(block_at(t)?),
                Flow::Branch(t) => Exit::Branch {
                    taken: block_at(t)?,
                    not_taken: fallthrough()?,
                },
                Flow::Stop if name == "s_setpc_b64" => Exit::Indirect,
                Flow::Stop => Exit::End,
            };
        }
        let m0 = m0_max.get(&machine.offset).copied();
        if let Some((mut reads, writes)) = effects(&machine, &fields, name, lanes, vgprs, m0)? {
            // Lanes disabled in EXEC keep the old value.
            for w in writes.iter().filter(|w| matches!(w, Reg::V(_))) {
                if !reads.contains(w) {
                    reads.push(*w);
                }
            }
            let unset = |regs: Vec<Reg>| regs.into_iter().map(|r| (r, NONE)).collect();
            blocks[b].insts.push(Inst {
                machine,
                fields,
                reads: unset(reads),
                writes: unset(writes),
            });
        }
    }
    link(&mut blocks);
    if !blocks[0].preds.is_empty() {
        return Err(ShaderError::Unsupported {
            offset: 0,
            what: "a branch to the program entry",
        });
    }
    split_irreducible(&mut blocks)?;

    Ok(rename(blocks, wave64, vgprs))
}

/// A fixed-lane access: save, VGPR, SGPR, lane.
type Park = (bool, u8, u8, u32);

/// Scalar registers the compiler parked in lanes of a VGPR, as scalar moves.
///
/// When it runs out of SGPRs the console compiler saves one in a lane of a
/// spare VGPR (`v_writelane_b32 vN, sX, K`) and takes it back later
/// (`v_readlane_b32 sX, vN, K`). A VGPR whose every use in the program is one
/// of those two forms with a constant lane is such a store: lane K of it is a
/// scalar register by another name. Each (VGPR, lane) gets a register code
/// the program does not use (the trap temporaries first, then SGPRs it never
/// names, then codes from [`PARKED`] up, which no machine word can hold), and
/// the save and the restore become `s_mov_b32` to and from it. Returns those
/// moves (index in `list`, destination code, source code) in order, for
/// `build` to put the codes into the decoded fields. They are then copies for
/// every later stage: a parked descriptor is traced to its load, and nothing
/// is left of a cross-lane operation, which a console wave run as two host
/// waves could not do where not every lane runs (the lane may be in the other
/// wave, or not running).
/// A register pair parked in two neighbouring lanes (a lane mask, a 64-bit
/// address) moves as a pair (`s_mov_b64`) when every save and every restore
/// of the two lanes come together, so a parked lane mask comes back as the
/// mask it is; otherwise its halves come back as two scalars.
/// If a relative read can also use the VGPR, only slots with saves that
/// cannot reach such a read and restores that always follow a save qualify.
fn unspill(
    list: &mut [gfx10::Instruction],
    leaders: &[usize],
    flows: &[Flow],
    wave64: bool,
    lanes: u8,
    vgprs: u16,
) -> Vec<(usize, u8, u8)> {
    /// `v_writelane_b32 vN, sX, K` / `v_readlane_b32 sX, vN, K`: (save, N, X, K).
    fn parked(inst: &gfx10::Instruction, fields: &Fields) -> Option<Park> {
        let Fields::Valu(v) = fields else { return None };
        if v.abs != 0 || v.neg != 0 || v.opsel != 0 || v.clamp || v.omod != 0 || v.sdwa.is_some() || v.dpp.is_some() {
            return None;
        }
        let lane = |op: &Operand| match *op {
            Operand::Const(c @ 128..=191) => Some(u32::from(c) - 128),
            _ => None,
        };
        match (inst.name()?, v.dst?, v.src.as_slice()) {
            ("v_writelane_b32", Operand::Vector { reg, .. }, [Operand::Scalar { code, .. }, k]) if *code < 105 => {
                Some((true, reg, *code, lane(k)?))
            }
            ("v_readlane_b32", Operand::Scalar { code, .. }, [Operand::Vector { reg, .. }, k]) if code < 105 => {
                Some((false, *reg, code, lane(k)?))
            }
            _ => None,
        }
    }
    // VGPRs used by parking alone, the scalar codes the program uses, and
    // per instruction the scalar codes it writes.
    let mut other = [false; 256];
    let mut relative_reads: [Vec<usize>; 256] = std::array::from_fn(|_| Vec::new());
    let mut used = [false; 128];
    let mut parks = Vec::with_capacity(list.len());
    let mut written: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut read: Vec<Vec<u8>> = Vec::with_capacity(list.len());
    let mut m0 = None;
    for (i, inst) in list.iter().enumerate() {
        if m0.is_some() && leaders.binary_search(&inst.offset).is_ok() {
            m0 = None;
        }
        let Some(name) = inst.name() else { return Vec::new() };
        let fields = inst.fields(wave64);
        let park = parked(inst, &fields);
        let Ok(effects) = effects(inst, &fields, name, lanes, vgprs, None) else {
            return Vec::new();
        };
        let mut codes = Vec::new();
        let mut from = Vec::new();
        if let Some((reads, writes)) = effects {
            for &reg in &writes {
                if let Reg::S(c) = reg {
                    codes.push(c);
                }
            }
            for &reg in &reads {
                if let Reg::S(c) = reg {
                    from.push(c);
                }
            }
            // A relative read only leaves a parking candidate untouched if
            // M0 is known to select a different VGPR. On an unknown or
            // out-of-range index, keep the full range from `effects`.
            let source = match (&fields, m0) {
                (Fields::Valu(v), Some(offset)) if name == "v_movrels_b32" => {
                    match v.src.as_slice() {
                        [Operand::Vector { reg, .. }] => reg
                            .checked_add(offset)
                            .filter(|&r| u16::from(r) < vgprs),
                        _ => None,
                    }
                }
                _ => None,
            };
            let n_reads = reads.len();
            for (k, reg) in reads.into_iter().chain(writes).enumerate() {
                if k < n_reads
                    && let Reg::V(r) = reg
                    && source.is_some_and(|src| src != r)
                {
                    continue;
                }
                match reg {
                    Reg::S(c) => used[usize::from(c)] = true,
                    Reg::V(r) if park.is_some_and(|p| p.1 == r) => {}
                    Reg::V(r) if k < n_reads && name == "v_movrels_b32" => {
                        relative_reads[usize::from(r)].push(i);
                    }
                    Reg::V(r) => other[usize::from(r)] = true,
                    Reg::Scc => {}
                }
            }
            if codes.contains(&M0) {
                m0 = match &fields {
                    Fields::Sop { src, .. } if name == "s_mov_b32" => match src.as_slice() {
                        [Operand::Const(c @ 128..=192)] => Some(c - 128),
                        [Operand::Literal(x)] => u8::try_from(*x).ok(),
                        _ => None,
                    },
                    _ => None,
                };
            }
        }
        parks.push(park);
        written.push(codes);
        read.push(from);
    }
    for park in &mut parks {
        if park.is_some_and(|p| other[usize::from(p.1)]) {
            *park = None;
        }
    }
    if parks.iter().all(Option::is_none) {
        return Vec::new();
    }
    let leader = |i: usize| leaders.binary_search(&list[i].offset).is_ok();
    // Lane pairs (register, lower lane) that always move together: each
    // save of the lower lane is followed, in the same block and before its
    // register changes, by the save of the next register into the next
    // lane; each restore of the lower lane is followed by the restore of
    // the next lane into the next register before any interfering access.
    // The second save may come any number of instructions after the first (the
    // compiler puts loads and constants between the two: five instructions later
    // in the ped mesh programs). Only wave64 programs have pairs: a wave32 mask
    // is one dword, and a pair move of it with its neighbour would hide it.
    let mut slots: Vec<(u8, u32)> = parks.iter().flatten().map(|p| (p.1, p.3)).collect();
    slots.sort_unstable();
    slots.dedup();
    // instruction -> the instruction that completes its pair
    let partner = |i: usize| -> Option<usize> {
        let (save, reg, code, lane) = parks[i]?;
        if save {
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((true, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        } else {
            // The second restore may come a few instructions later (the
            // compiler schedules other work between the two): the pair is
            // restored at the first, so nothing between may read or write
            // the second register, write the first, or save into the lanes.
            let mut j = i + 1;
            while j < list.len() && !leader(j) {
                if parks[j] == Some((false, reg, code + 1, lane + 1)) {
                    return Some(j);
                }
                if parks[j].is_some_and(|p| p.0 && p.1 == reg && (p.3 == lane || p.3 == lane + 1))
                    || written[j].contains(&code)
                    || written[j].contains(&(code + 1))
                    || read[j].contains(&(code + 1))
                {
                    return None;
                }
                j += 1;
            }
            None
        }
    };
    let mut pairs: Vec<(u8, u32)> = Vec::new();
    for &(reg, lane) in &slots {
        if !wave64 || pairs.iter().any(|&(r, l)| r == reg && l + 1 == lane) || !slots.contains(&(reg, lane + 1)) {
            continue;
        }
        let lower: Vec<usize> = (0..list.len()).filter(|&i| parks[i].is_some_and(|p| (p.1, p.3) == (reg, lane))).collect();
        let upper = parks.iter().flatten().filter(|p| (p.1, p.3) == (reg, lane + 1)).count();
        let partners: Vec<Option<usize>> = lower.iter().map(|&i| partner(i)).collect();
        let mut distinct: Vec<usize> = partners.iter().flatten().copied().collect();
        distinct.sort_unstable();
        distinct.dedup();
        if partners.iter().all(Option::is_some) && distinct.len() == lower.len() && upper == lower.len() {
            pairs.push((reg, lane));
        }
    }
    let mut blocked = blocked_mixed_parks(list, flows, &parks, &slots, &relative_reads, wave64);
    // Discover pairs before filtering slots so that a half that needs to
    // remain in the VGPR also keeps its partner there.
    pairs.retain(|&(reg, lane)| {
        let pair = 3u64 << lane;
        if blocked[usize::from(reg)] & pair == 0 {
            return true;
        }
        blocked[usize::from(reg)] |= pair;
        false
    });
    let physical = |reg: u8, lane| blocked[usize::from(reg)] & (1u64 << lane) != 0;
    slots.retain(|&(reg, lane)| !physical(reg, lane));
    // Codes for the parked scalars: the trap temporaries (108..=123), then
    // user SGPRs the program never names, from the top, then the codes past
    // the machine's (`PARKED`..=254; 255 stays unused so that the code after a
    // parked one always exists). A material resolve program parks up to 63
    // scalars and leaves about 20 machine codes unused.
    let past = usize::from(PARKED);
    let mut free = [false; 256];
    for c in (0..105).chain(108..=123) {
        free[c] = !used[c];
    }
    free[past..255].fill(true);
    let mut codes: Vec<((u8, u32), u8)> = Vec::new();
    for &(reg, lane) in &pairs {
        let at = (108..123).chain((0..104).rev()).chain(past..254).find(|&c| free[c] && free[c + 1]);
        // Not enough spare codes: leave the program as it is.
        let Some(c) = at else { return Vec::new() };
        (free[c], free[c + 1]) = (false, false);
        codes.push(((reg, lane), c as u8));
        codes.push(((reg, lane + 1), c as u8 + 1));
    }
    for &slot in &slots {
        if codes.iter().any(|c| c.0 == slot) {
            continue;
        }
        let Some(c) = (108..=123).chain((0..105).rev()).chain(past..255).find(|&c| free[c]) else {
            return Vec::new();
        };
        free[c] = false;
        codes.push((slot, c as u8));
    }
    let code_of = |reg: u8, lane: u32| codes.iter().find(|c| c.0 == (reg, lane)).map(|c| c.1);
    let put = |list: &mut [gfx10::Instruction], i: usize, encoding, opcode: u16, word: u32| {
        list[i] = gfx10::Instruction::new(list[i].offset, encoding, opcode, vec![word]);
    };
    // `s_mov_b32` / `s_mov_b64 dst, src` (SOP1, opcodes 3 and 4); `s_nop`. A code
    // from `PARKED` up does not fit the word: s0 stands in for it there.
    let field = |code: u8| if code < PARKED { u32::from(code) } else { 0 };
    let mov = |wide: bool, dst: u8, src: u8| 0xBE80_0000 | field(dst) << 16 | (3 + u32::from(wide)) << 8 | field(src);
    const NOP: u32 = 0xBF80_0000;
    type Edit = (usize, Option<(bool, u8, u8)>);
    let mut edits: Vec<Edit> = Vec::new();
    for (i, &park) in parks.iter().enumerate().take(list.len()) {
        let Some((save, reg, code, lane)) = park else {
            continue;
        };
        if physical(reg, lane) {
            continue;
        }
        let Some(slot) = code_of(reg, lane) else { return Vec::new() };
        if pairs.contains(&(reg, lane)) {
            let Some(j) = partner(i) else { return Vec::new() };
            // A save moves the pair where its second half is saved; a
            // restore where its first half is restored.
            let (at, gone) = if save { (j, i) } else { (i, j) };
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((at, Some((true, dst, src))));
            edits.push((gone, None));
        } else if lane > 0 && pairs.contains(&(reg, lane - 1)) {
            // the upper half of a pair: moved with the lower
        } else {
            let (dst, src) = if save { (slot, code) } else { (code, slot) };
            edits.push((i, Some((false, dst, src))));
        }
    }
    let mut moves = Vec::new();
    for (i, edit) in edits {
        match edit {
            Some((wide, dst, src)) => {
                put(list, i, gfx10::Encoding::Sop1, 3 + u16::from(wide), mov(wide, dst, src));
                moves.push((i, dst, src));
            }
            None => put(list, i, gfx10::Encoding::Sopp, 0, NOP),
        }
    }
    moves.sort_unstable();
    moves
}

/// Fixed-lane slots that a relative source may observe, or whose restores
/// can reach a path with no prior save. Only carriers with a relative use
/// need the extra checks.
fn blocked_mixed_parks(
    list: &[Instruction],
    flows: &[Flow],
    parks: &[Option<Park>],
    slots: &[(u8, u32)],
    relative_reads: &[Vec<usize>; 256],
    wave64: bool,
) -> [u64; 256] {
    let mut mixed = [0u64; 256];
    for &(reg, lane) in slots {
        let reg = usize::from(reg);
        if !relative_reads[reg].is_empty() {
            mixed[reg] |= 1u64 << lane;
        }
    }
    let mut blocked = [0u64; 256];
    if mixed.iter().all(|&m| m == 0) {
        return blocked;
    }
    let mut successors = Vec::with_capacity(list.len());
    let mut predecessors = vec![Vec::new(); list.len()];
    for (i, &flow) in flows.iter().enumerate() {
        let next = (i + 1 < list.len()).then_some(i + 1);
        let edge = |offset| list.binary_search_by_key(&offset, |inst| inst.offset).ok();
        let edges = match flow {
            Flow::Next => [next, None],
            // An unknown transfer might reach a relative reader.
            Flow::Stop if list[i].name() == Some("s_setpc_b64") => return mixed,
            Flow::Stop => [None, None],
            Flow::Jump(t) => {
                let Some(target) = edge(t) else { return mixed };
                [Some(target), None]
            }
            Flow::Branch(t) => {
                let Some(target) = edge(t) else { return mixed };
                [Some(target), next]
            }
        };
        for target in edges.into_iter().flatten() {
            predecessors[target].push(i);
        }
        successors.push(edges);
    }
    for (reg, slots) in mixed.into_iter().enumerate().filter(|&(_, m)| m != 0) {
        if !wave64 && slots >> 32 != 0 {
            blocked[reg] = slots;
            continue;
        }
        let mut observed = vec![false; list.len()];
        let mut work = relative_reads[reg].clone();
        while let Some(i) = work.pop() {
            if std::mem::replace(&mut observed[i], true) {
                continue;
            }
            if let Some((true, vgpr, _, lane)) = parks[i]
                && usize::from(vgpr) == reg
            {
                blocked[reg] |= 1u64 << lane;
            }
            work.extend(&predecessors[i]);
        }

        // A bit follows every path along which that slot has not been
        // initialized. Union at joins exposes even a single bypass path.
        let mut unsaved = vec![0; list.len()];
        unsaved[0] = slots & !blocked[reg];
        work.push(0);
        while let Some(i) = work.pop() {
            let mut out = unsaved[i];
            if let Some((save, vgpr, _, lane)) = parks[i]
                && usize::from(vgpr) == reg
            {
                let lane = 1u64 << lane;
                if save {
                    out &= !lane;
                } else {
                    blocked[reg] |= out & lane;
                }
            }
            for next in successors[i].into_iter().flatten() {
                let joined = unsaved[next] | out;
                if joined != unsaved[next] {
                    unsaved[next] = joined;
                    work.push(next);
                }
            }
        }
    }
    blocked
}

/// Rename register reads and writes after changing edges in a reducible graph.
fn rename(mut blocks: Vec<Block>, wave64: bool, vgprs: u16) -> Program {
    let order = reverse_postorder(&blocks);
    let (live_in, live_out) = liveness(&blocks, &order);
    let mut values = 0u32;
    let mut new_value = || {
        values += 1;
        Value(values - 1)
    };
    let mut inputs = Vec::new();
    let mut outs: Vec<Vec<(Reg, Value)>> = vec![Vec::new(); blocks.len()];
    let mut current = [NONE; REGS];
    for &b in &order {
        current.fill(NONE);
        let block = &mut blocks[b];
        for reg in members(&live_in[b]) {
            current[index(reg)] = match block.preds[..] {
                [] => {
                    let v = new_value();
                    inputs.push((reg, v));
                    v
                }
                // A single predecessor comes earlier in reverse postorder.
                [p] => lookup(&outs[p], reg),
                _ => {
                    let value = new_value();
                    block.phis.push(Phi {
                        reg,
                        value,
                        args: Vec::new(),
                    });
                    value
                }
            };
        }
        for inst in &mut block.insts {
            for (reg, v) in &mut inst.reads {
                *v = current[index(*reg)];
                debug_assert_ne!(*v, NONE, "{reg:?} read at {:#x}", inst.machine.offset);
            }
            for (reg, v) in &mut inst.writes {
                *v = new_value();
                current[index(*reg)] = *v;
            }
        }
        outs[b] = members(&live_out[b])
            .map(|reg| (reg, current[index(reg)]))
            .collect();
    }
    for block in &mut blocks {
        for phi in &mut block.phis {
            phi.args = block
                .preds
                .iter()
                .map(|&p| lookup(&outs[p], phi.reg))
                .collect();
            debug_assert!(!phi.args.contains(&NONE), "{phi:?}");
        }
    }
    remove_trivial_phis(&mut blocks, values);
    Program {
        wave64,
        vgprs,
        blocks,
        inputs,
    }
}

/// Duplicate a join for each proven (predecessor, join, destination) edge.
/// The instructions still execute on that path and define fresh SSA values.
pub(crate) fn thread_edges(
    p: &Program,
    edges: &[(usize, usize, usize)],
) -> Result<Program, ShaderError> {
    let mut blocks = p.blocks.clone();
    for block in &mut blocks {
        block.phis.clear();
    }
    for &(pred, join, dest) in edges {
        let mut copy = blocks[join].clone();
        copy.exit = Exit::Jump(dest);
        let target = blocks.len();
        match &mut blocks[pred].exit {
            Exit::Jump(t) => *t = target,
            Exit::Branch { taken, not_taken } => {
                if *taken == join {
                    *taken = target;
                }
                if *not_taken == join {
                    *not_taken = target;
                }
            }
            _ => unreachable!("threading an incoming edge"),
        }
        blocks.push(copy);
    }
    link(&mut blocks);
    split_irreducible(&mut blocks)?;
    Ok(rename(blocks, p.wave64, p.vgprs))
}

/// Blocks reached by falling through the branch and stopping at its skip
/// destination, and their membership for testing incoming edges at the join.
fn skipped_region(p: &Program, branch: usize, first: usize, target: usize) -> Option<(Vec<usize>, Vec<bool>)> {
    let mut seen = vec![false; p.blocks.len()];
    let mut pending = vec![first];
    let mut region = Vec::new();
    while let Some(b) = pending.pop() {
        if b == target || seen[b] {
            continue;
        }
        if b == branch {
            return None;
        }
        seen[b] = true;
        region.push(b);
        pending.extend(p.blocks[b].exit.successors());
    }
    // A saved value used here must have been defined during this walk.
    // With other entrances a loop could carry one from before the skip.
    if region.iter().any(|&b| p.blocks[b].preds.iter().any(|&pred| !seen[pred] && pred != branch)) {
        return None;
    }
    Some((region, seen))
}

/// Whether the lanes that would have skipped the region of the
/// `s_cbranch_execz` ending `branch` (the blocks `inside` marks, from
/// [`skipped_region`]) can walk it with EXEC clear and leave nothing behind but
/// registers, as when the console takes the branch with no lane left.
///
/// Clear means provably zero, by SSA value: EXEC as the branch reads it; the
/// mask a `*_saveexec_*` saves where EXEC is clear, and the one a compare
/// writes there (lanes outside EXEC get 0); AND, ANDN1, ANDN2, OR and XOR
/// (plain, `saveexec` and `wrexec`), moves and selects of clear operands (0, or
/// clear values) where those give zero; joins of clear values. A loop's joins
/// start out clear and stop being so when a way in brings something else,
/// until nothing changes.
///
/// EXEC may open where the compiler takes the whole wave for a moment (a wave
/// sum behind a bounds test: `s_or_saveexec_b64 s[4:5], -1` ... `s_mov_b64
/// exec, s[4:5]`), if the mask that opening saved puts it back before its
/// block ends and nothing in between writes more than registers: the registers
/// written there are ones the compiler takes as overwritten in every lane, as
/// it must whenever that code runs. Only that opening is taken
/// ([`whole_wave`]), narrowed inside by constants alone ([`fixed_mask`]: a
/// prefix sum's `s_mov_b32 exec_lo, 0xffff0000`). A restore of a mask from
/// before the branch runs the lanes of that mask, and what they write to
/// registers is read after the join where the console, which skipped the
/// region, still has what was there (v1 = 7 before the branch, 9 written under
/// the restored mask, the join's store of v1), so it keeps the branch.
///
/// The walk also has to end: a host lane decides a branch on a mask by its own
/// bit, so one outside EXEC never leaves a loop that goes on "while no running
/// lane wants out" ([`spins_with_empty_exec`]).
fn walks_with_empty_exec(p: &Program, branch: usize, inside: &[bool]) -> bool {
    let lanes = 1 + u8::from(p.wave64);
    let Some(test) = p.blocks[branch].insts.last() else {
        return false;
    };
    let order: Vec<usize> = reverse_postorder(&p.blocks)
        .into_iter()
        .filter(|&b| inside[b])
        .collect();
    // Every scalar the region writes starts out clear, and what cannot be shown
    // to be leaves, until nothing does.
    let mut zero: std::collections::HashSet<Value> = (0..lanes)
        .filter_map(|k| test.read(Reg::S(EXEC + k)))
        .collect();
    for &b in &order {
        let block = &p.blocks[b];
        let phis = block.phis.iter().map(|phi| (phi.reg, phi.value));
        let writes = block
            .insts
            .iter()
            .flat_map(|inst| inst.writes.iter().copied());
        zero.extend(
            phis.chain(writes)
                .filter(|w| matches!(w.0, Reg::S(_)))
                .map(|w| w.1),
        );
    }
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                if zero.contains(&phi.value) && !phi.args.iter().all(|a| zero.contains(a)) {
                    zero.remove(&phi.value);
                    changed = true;
                }
            }
            for inst in &block.insts {
                for (i, &(reg, value)) in inst.writes.iter().enumerate() {
                    if matches!(reg, Reg::S(_))
                        && zero.contains(&value)
                        && !writes_zero(inst, i, &zero)
                    {
                        zero.remove(&value);
                        changed = true;
                    }
                }
            }
        }
    }
    // Each block starts with EXEC clear: the region does, and every block of it
    // ends so.
    for &b in &order {
        // The register a whole-wave opening saved the clear EXEC in, and the
        // values it holds, while that window is open.
        let mut window: Option<(u8, [Option<Value>; 2])> = None;
        for inst in &p.blocks[b].insts {
            if window.is_some() && leaves_registers(inst) {
                return false;
            }
            let written: Vec<Value> = (0..lanes)
                .filter_map(|k| inst.write(Reg::S(EXEC + k)))
                .collect();
            if written.is_empty() {
                continue;
            }
            let clear = written.iter().all(|v| zero.contains(v));
            window = match window {
                None if clear => None,
                // EXEC opens for the whole wave only ...
                None => match whole_wave(inst, lanes) {
                    Some(saved) => Some(saved),
                    None => return false,
                },
                // ... and until the mask that opening saved puts it back, only
                // fixed masks narrow it.
                Some((code, saved)) => {
                    let back = (0..lanes).all(|k| {
                        let held = saved[usize::from(k)];
                        held.is_some() && inst.read(Reg::S(code + k)) == held
                    });
                    match (clear && back, fixed_mask(inst)) {
                        (true, _) => None,
                        (false, true) => Some((code, saved)),
                        (false, false) => return false,
                    }
                }
            };
        }
        if window.is_some() {
            return false;
        }
    }
    let Exit::Branch { not_taken: first, .. } = p.blocks[branch].exit else {
        return false;
    };
    !spins_with_empty_exec(p, first, inside, &zero)
}

/// The register in which `inst` saves EXEC while it opens it to the whole wave
/// (`s_or_saveexec sD, -1` or `s_nand_saveexec sD, 0`, the openings
/// `emit::wave` takes its sections from), and the values saved there by dword.
fn whole_wave(inst: &Inst, lanes: u8) -> Option<(u8, [Option<Value>; 2])> {
    let all = match inst.name() {
        "s_or_saveexec_b64" | "s_or_saveexec_b32" => Operand::Const(193),
        "s_nand_saveexec_b64" | "s_nand_saveexec_b32" => Operand::Const(128),
        _ => return None,
    };
    let Fields::Sop {
        dst: Some(Operand::Scalar { code, dwords }),
        src,
        ..
    } = &inst.fields
    else {
        return None;
    };
    if src.first() != Some(&all) || *dwords != lanes {
        return None;
    }
    let mut saved = [None; 2];
    for k in 0..lanes {
        saved[usize::from(k)] = Some(inst.write(Reg::S(code + k))?);
    }
    Some((*code, saved))
}

/// Whether `inst` sets EXEC, or one dword of it, to a constant (`s_mov_b32
/// exec_lo, 0xffff0000`, as the steps of a prefix sum over the wave do).
fn fixed_mask(inst: &Inst) -> bool {
    matches!(inst.name(), "s_mov_b32" | "s_mov_b64")
        && matches!(
            &inst.fields,
            Fields::Sop { dst: Some(Operand::Scalar { code, .. }), src, .. }
                if (EXEC..EXEC + 2).contains(code)
                    && matches!(src.first(), Some(Operand::Const(128..=208) | Operand::Literal(_)))
        )
}

/// Where the branch that ends block `b` goes for a lane outside EXEC, if the
/// values in `zero` decide that: a branch on EXEC, on a VCC that is clear, or
/// on the SCC of a mask operation whose result is clear (`scc`: the
/// instruction that writes each SCC value). `None` if it turns on anything
/// else, which may let the lane out.
fn decided(
    p: &Program,
    b: usize,
    zero: &std::collections::HashSet<Value>,
    scc: &std::collections::HashMap<Value, &Inst>,
) -> Option<usize> {
    let block = &p.blocks[b];
    let Exit::Branch { taken, not_taken } = block.exit else {
        return None;
    };
    let inst = block.insts.last()?;
    let lanes = 1 + u8::from(p.wave64);
    let clear = |inst: &Inst, code: u8| {
        (0..lanes).all(|k| inst.read(Reg::S(code + k)).is_some_and(|v| zero.contains(&v)))
    };
    match inst.name() {
        "s_cbranch_execz" if clear(inst, EXEC) => Some(taken),
        "s_cbranch_execnz" if clear(inst, EXEC) => Some(not_taken),
        "s_cbranch_vccz" if clear(inst, VCC) => Some(taken),
        "s_cbranch_vccnz" if clear(inst, VCC) => Some(not_taken),
        name @ ("s_cbranch_scc0" | "s_cbranch_scc1") => {
            // SCC of a mask operation: whether its result (EXEC, for the forms
            // that write EXEC) is not zero.
            let def = scc.get(&inst.read(Reg::Scc)?)?;
            let op = def.name();
            let mask = matches!(def.fields, Fields::Sop { .. })
                && ["s_and_", "s_or_", "s_xor_", "s_andn2_", "s_andn1_"]
                    .iter()
                    .any(|prefix| op.starts_with(prefix))
                && (op.ends_with("_b64") || op.ends_with("_b32"));
            if !mask {
                return None;
            }
            let zeroed = if op.contains("saveexec") || op.contains("wrexec") {
                (0..lanes).all(|k| def.write(Reg::S(EXEC + k)).is_some_and(|v| zero.contains(&v)))
            } else {
                let mut results = def.writes.iter().filter(|w| matches!(w.0, Reg::S(_)));
                let mut any = false;
                let all = results.all(|w| {
                    any = true;
                    zero.contains(&w.1)
                });
                any && all
            };
            zeroed.then_some(if name == "s_cbranch_scc0" { taken } else { not_taken })
        }
        _ => None,
    }
}

/// Whether a lane that walks the region (`inside`) from `first` with EXEC
/// clear can come to a loop it never leaves: blocks that jumps and branches
/// decided by the clear masks ([`decided`]) lead round. The console's wave
/// leaves such a loop with its last running lane; a host lane outside EXEC
/// decides by its own bit and goes round until the lowering's limit, and past
/// it runs what follows the loop with no guard (c4183d3c,
/// A projected-decal resolve program (c4183d3c): `s_andn2_b64 vcc, exec, vcc`, `s_cbranch_vccz`
/// back). A branch on a scalar counts as a way out.
fn spins_with_empty_exec(
    p: &Program,
    first: usize,
    inside: &[bool],
    zero: &std::collections::HashSet<Value>,
) -> bool {
    let n = p.blocks.len();
    let mut scc = std::collections::HashMap::new();
    for inst in p.blocks.iter().flat_map(|block| &block.insts) {
        if let Some(value) = inst.write(Reg::Scc) {
            scc.insert(value, inst);
        }
    }
    // The one way on from a block of the region, where there is only one.
    let only: Vec<Option<usize>> = (0..n)
        .map(|b| match p.blocks[b].exit {
            _ if !inside[b] => None,
            Exit::Jump(to) => Some(to),
            Exit::Branch { .. } => decided(p, b, zero, &scc),
            _ => None,
        })
        .collect();
    // The blocks the walk may come to.
    let mut reached = vec![false; n];
    let mut pending = vec![first];
    while let Some(b) = pending.pop() {
        if b >= n || !inside[b] || reached[b] {
            continue;
        }
        reached[b] = true;
        match only[b] {
            Some(to) => pending.push(to),
            None => pending.extend(p.blocks[b].exit.successors()),
        }
    }
    // A way round among them (0: not looked at, 1: on this path, 2: leads out).
    let mut state = vec![0u8; n];
    for start in (0..n).filter(|&b| reached[b]) {
        let mut path = Vec::new();
        let mut b = start;
        loop {
            match state[b] {
                1 => return true,
                2 => break,
                _ => {}
            }
            state[b] = 1;
            path.push(b);
            match only[b] {
                Some(to) if to < n && inside[to] => b = to,
                _ => break,
            }
        }
        for b in path {
            state[b] = 2;
        }
    }
    false
}

/// Whether the `i`th register `inst` writes is zero where the values in `zero`
/// are ([`walks_with_empty_exec`]).
fn writes_zero(inst: &Inst, i: usize, zero: &std::collections::HashSet<Value>) -> bool {
    let reg = inst.writes[i].0;
    let Reg::S(code) = reg else {
        return false;
    };
    // (a register written twice, a mask saved into EXEC itself, is not followed)
    if inst.writes.iter().filter(|w| w.0 == reg).count() > 1 {
        return false;
    }
    let clear = |v: Option<Value>| v.is_some_and(|v| zero.contains(&v));
    let exec = |k: u8| clear(inst.read(Reg::S(EXEC + k)));
    // Dword `k` of an operand.
    let operand = |op: Option<&Operand>, k: u8| match op {
        Some(Operand::Const(128) | Operand::Literal(0)) => true,
        Some(&Operand::Scalar { code, .. }) if code == gfx10::NULL => true,
        Some(&Operand::Scalar { code, dwords }) => k < dwords && clear(inst.read(Reg::S(code + k))),
        _ => false,
    };
    // Which dword of an operand naming `dwords` registers from `first` this is.
    let dword = |first: u8, dwords: u8| code.checked_sub(first).filter(|&k| k < dwords);
    let name = inst.name();
    match &inst.fields {
        // A compare clears the bits of the lanes outside EXEC; one into EXEC
        // only keeps fewer.
        Fields::Valu(_) if name.starts_with("v_cmpx_") => dword(EXEC, 2).is_some_and(exec),
        Fields::Valu(v) if name.starts_with("v_cmp_") => match v.dst {
            Some(Operand::Scalar {
                code: first,
                dwords,
            }) => dword(first, dwords).is_some_and(exec),
            _ => false,
        },
        Fields::Sop { dst, src, .. } => {
            let Some(op) = name
                .strip_prefix("s_")
                .and_then(|n| n.strip_suffix("_b32").or(n.strip_suffix("_b64")))
            else {
                return false;
            };
            // `D = EXEC; EXEC = f(S0, EXEC)`; a `wrexec` saves the new EXEC.
            let (f, save) = match (op.strip_suffix("_saveexec"), op.strip_suffix("_wrexec")) {
                (Some(f), _) => (f, Some(true)),
                (_, Some(f)) => (f, Some(false)),
                _ => (op, None),
            };
            let result = |k: u8| {
                let a = operand(src.first(), k);
                let b = if save.is_some() {
                    exec(k)
                } else {
                    operand(src.get(1), k)
                };
                match f {
                    "and" => a || b,
                    "andn2" | "mov" => a,
                    "andn1" => b,
                    "or" | "xor" | "cselect" => a && b,
                    _ => false,
                }
            };
            let to_dst = match *dst {
                Some(Operand::Scalar {
                    code: first,
                    dwords,
                }) => dword(first, dwords),
                _ => None,
            };
            match (save, to_dst) {
                (Some(true), Some(k)) => exec(k),
                (_, Some(k)) => result(k),
                (Some(_), None) => dword(EXEC, 2).is_some_and(result),
                (None, None) => false,
            }
        }
        _ => false,
    }
}

/// Whether `inst` changes more than registers: memory (LDS and GDS, buffers,
/// images, flat, global and scratch), an export or a message.
fn leaves_registers(inst: &Inst) -> bool {
    let name = inst.name();
    match &inst.fields {
        Fields::Ds { .. } | Fields::Exp { .. } => true,
        Fields::Buffer { lds, .. } | Fields::Flat { lds, .. } => *lds || !name.contains("_load"),
        Fields::Image { .. } => name.contains("_store") || name.contains("_atomic"),
        Fields::Smem { .. } => {
            name.contains("_store") || name.contains("_atomic") || name == "s_dcache_wb"
        }
        Fields::Sopp { .. } => name.starts_with("s_sendmsg"),
        _ => false,
    }
}

/// The program without its early-outs: an `s_cbranch_execz` to a block that
/// only ends the program (`if (!inBounds) return;`, the outermost branch of
/// most compute programs) is left out, so the lanes that would have left
/// walk the body with EXEC clear. EXEC must stay clear in the region, but for
/// a whole-wave moment that only writes registers ([`walks_with_empty_exec`]):
/// a restore there of a mask from before the branch would run vector
/// operations for those lanes. The body is then code every lane runs, which a
/// console wave run as two host waves needs for the barriers of its exchanges
/// and LDS accesses.
/// `None` if the program has no such branch.
pub(crate) fn without_early_outs(p: &Program) -> Option<Program> {
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b].insts.iter().all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    let mut blocks = p.blocks.clone();
    let mut any = false;
    for (b, block) in blocks.iter_mut().enumerate() {
        let Exit::Branch { taken, not_taken } = block.exit else {
            continue;
        };
        if ends(taken) && block.insts.last().is_some_and(|i| i.name() == "s_cbranch_execz")
            && skipped_region(p, b, not_taken, taken).is_some_and(|(_, inside)| walks_with_empty_exec(p, b, &inside)) {
            block.insts.pop();
            block.exit = Exit::Jump(not_taken);
            any = true;
        }
    }
    if !any {
        return None;
    }
    for block in &mut blocks {
        block.phis.clear();
    }
    link(&mut blocks);
    split_irreducible(&mut blocks).ok()?;
    Some(rename(blocks, p.wave64, p.vgprs))
}

/// As [`without_early_outs`], and also without the branches that only skip a
/// region when no lane is left for it: an `s_cbranch_execz` whose target is
/// where the skipped code ends up anyway (`if (c) { ... }` with nothing
/// else leaving the region). Every lane then walks the region with EXEC
/// clear for those that would have skipped it, and LDS accesses and
/// exchanges inside it are in code every lane runs.
/// The console takes such a branch only when no lane of the wave is left,
/// and then its scalar code does not run: a branch is kept if the code
/// after it could see that (a scalar register or SCC that the region
/// changes and the join merges with the value from before; EXEC aside,
/// which is empty either way). `None` if nothing changes.
///
/// `deep` looks closer at what the code after the branch can see. A scalar the region moves
/// away and back (the compiler parks scalars in VGPR lanes around a branch and takes them
/// back before the join, `unspill`) is the value it copies; a join nothing reads shows no
/// one the difference (a restore of EXEC that the next one overwrites reads nothing); and
/// the join may have other ways in (an inner branch that ends together with an outer one).
/// That leaves out most of the skips of the title's large lighting programs, whose bodies
/// are then code every lane runs.
pub(crate) fn without_skips(p: &Program, deep: bool) -> Option<Program> {
    let n = p.blocks.len();
    let ends = |b: usize| {
        p.blocks[b].exit == Exit::End
            && p.blocks[b].insts.iter().all(|i| matches!(i.fields, crate::gfx10::Fields::Sopp { .. }))
    };
    // Scalars moved from register to register (a scalar the compiler parked in a VGPR lane
    // comes back by such a move, `unspill`) are the value they copy.
    let mut copy: std::collections::HashMap<Value, Value> = Default::default();
    // Values something reads, directly or through joins. A scalar operation none of whose
    // results is read reads nothing either (the compiler restores EXEC from a saved mask
    // and overwrites it at once where two branches end together).
    let mut live: std::collections::HashSet<Value> = Default::default();
    let idle = |inst: &Inst, live: &std::collections::HashSet<Value>| {
        let name = inst.name();
        matches!(inst.fields, crate::gfx10::Fields::Sop { .. })
            && !(name.starts_with("s_setpc")
                || name.starts_with("s_swappc")
                || name.starts_with("s_call")
                || name.starts_with("s_rfe")
                || name.starts_with("s_movrel")
                || name.starts_with("s_subvector"))
            && inst.writes.iter().all(|w| !live.contains(&w.1))
    };
    for block in &p.blocks {
        for inst in &block.insts {
            if matches!(inst.name(), "s_mov_b32" | "s_mov_b64")
                && inst.reads.len() == inst.writes.len()
                && inst.reads.iter().chain(&inst.writes).all(|r| matches!(r.0, Reg::S(_)))
            {
                copy.extend(inst.writes.iter().zip(&inst.reads).map(|(w, r)| (w.1, r.1)));
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for block in &p.blocks {
            for inst in &block.insts {
                if !idle(inst, &live) {
                    for r in &inst.reads {
                        changed |= live.insert(r.1);
                    }
                }
            }
            for phi in &block.phis {
                if live.contains(&phi.value) {
                    for &a in &phi.args {
                        changed |= live.insert(a);
                    }
                }
            }
        }
    }
    let root = |mut v: Value| {
        while deep && let Some(&c) = copy.get(&v) {
            v = c;
        }
        v
    };
    let scalars_unchanged = |b: usize, target: usize, inside: &[bool]| {
        // the join sees no scalar the region changed
        let join = &p.blocks[target];
        for phi in &join.phis {
            let scalar = match phi.reg {
                Reg::S(c) => c != 126 && c != 127,
                Reg::Scc => true,
                Reg::V(_) => false,
            };
            // (a join nothing reads shows no one the difference)
            if !scalar || deep && !live.contains(&phi.value) {
                continue;
            }
            let before: Vec<Value> =
                join.preds.iter().zip(&phi.args).filter(|(q, _)| **q == b).map(|(_, a)| root(*a)).collect();
            let within = join.preds.iter().zip(&phi.args).filter(|(q, _)| inside[**q]).map(|(_, a)| root(*a));
            if within.into_iter().any(|a| !before.contains(&a)) {
                return false;
            }
        }
        // Other ways into the join than the branch and the region (an inner branch that ends
        // together with an outer one) change nothing in that: the lanes that would have
        // skipped arrive from the region instead, with the same scalars.
        deep || join.preds.iter().all(|&q| q == b || inside[q])
    };
    let mut blocks = p.blocks.clone();
    let mut any = false;
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        let Exit::Branch { taken, not_taken } = block.exit else {
            continue;
        };
        if !block.insts.last().is_some_and(|i| i.name() == "s_cbranch_execz") {
            continue;
        }
        let Some((region, inside)) = skipped_region(p, b, not_taken, taken) else { continue };
        let pure = region.iter().all(|&x| !matches!(p.blocks[x].exit, Exit::End | Exit::Indirect));
        if (ends(taken) || pure && scalars_unchanged(b, taken, &inside)) && walks_with_empty_exec(p, b, &inside) {
            blocks[b].insts.pop();
            blocks[b].exit = Exit::Jump(not_taken);
            any = true;
        }
    }
    if !any {
        return None;
    }
    for block in &mut blocks {
        block.phis.clear();
    }
    link(&mut blocks);
    split_irreducible(&mut blocks).ok()?;
    Some(rename(blocks, p.wave64, p.vgprs))
}

/// Placeholder before renaming.
const NONE: Value = Value(u32::MAX);
/// Scalar codes 0-255 (from [`PARKED`] up: parked scalars), SCC, VGPRs 0-255.
const REGS: usize = 256 + 1 + 256;
type RegSet = [u64; REGS.div_ceil(64)];

fn index(reg: Reg) -> usize {
    match reg {
        Reg::S(c) => usize::from(c),
        Reg::Scc => 256,
        Reg::V(r) => 257 + usize::from(r),
    }
}

fn reg_at(i: usize) -> Reg {
    match i {
        0..256 => Reg::S(i as u8),
        256 => Reg::Scc,
        _ => Reg::V((i - 257) as u8),
    }
}

fn insert(set: &mut RegSet, reg: Reg) {
    let i = index(reg);
    set[i / 64] |= 1 << (i % 64);
}

fn contains(set: &RegSet, reg: Reg) -> bool {
    let i = index(reg);
    set[i / 64] >> (i % 64) & 1 != 0
}

/// In [`Reg`] order, which is also index order.
fn members(set: &RegSet) -> impl Iterator<Item = Reg> + '_ {
    (0..REGS)
        .filter(|&i| set[i / 64] >> (i % 64) & 1 != 0)
        .map(reg_at)
}

/// `outs` is sorted by register, and holds every register live out.
fn lookup(outs: &[(Reg, Value)], reg: Reg) -> Value {
    outs.binary_search_by_key(&reg, |&(r, _)| r)
        .map_or(NONE, |i| outs[i].1)
}

pub(crate) fn reverse_postorder(blocks: &[Block]) -> Vec<usize> {
    let mut seen = vec![false; blocks.len()];
    let mut post = Vec::with_capacity(blocks.len());
    let mut stack = vec![(0, 0)];
    seen[0] = true;
    while let Some(top) = stack.last_mut() {
        let (b, i) = *top;
        top.1 += 1;
        match blocks[b].exit.successors().get(i) {
            Some(&s) if !seen[s] => {
                seen[s] = true;
                stack.push((s, 0));
            }
            Some(_) => {}
            None => {
                post.push(b);
                stack.pop();
            }
        }
    }
    post.reverse();
    post
}

/// Recomputes every block's predecessors from the exits.
fn link(blocks: &mut [Block]) {
    for block in blocks.iter_mut() {
        block.preds.clear();
    }
    for b in 0..blocks.len() {
        for s in blocks[b].exit.successors() {
            blocks[s].preds.push(b);
        }
    }
}

/// An edge to an earlier block in reverse postorder whose target does not
/// dominate its source, if any: there the control flow is irreducible.
/// `index` is each block's position in `order`.
pub(crate) fn irreducible_edge(
    blocks: &[Block],
    order: &[usize],
    index: &[usize],
) -> Option<(usize, usize)> {
    let &entry = order.first()?;
    let idom = idoms(blocks, order, index);
    for &b in order {
        for s in blocks[b].exit.successors() {
            if index[s] > index[b] {
                continue;
            }
            let mut d = b;
            while d != s && d != entry {
                d = idom[d];
            }
            if d != s {
                return Some((b, s));
            }
        }
    }
    None
}

/// Each block's immediate dominator (the entry's is itself, unreachable
/// blocks' `usize::MAX`); `order` is reverse postorder, `index` each
/// block's position in it.
pub(crate) fn idoms(blocks: &[Block], order: &[usize], index: &[usize]) -> Vec<usize> {
    let mut idom = vec![usize::MAX; blocks.len()];
    let Some(&entry) = order.first() else {
        return idom;
    };
    idom[entry] = entry;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| {
        while a != b {
            while index[a] > index[b] {
                a = idom[a];
            }
            while index[b] > index[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order[1..] {
            let mut new = usize::MAX;
            for &q in blocks[b].preds.iter().filter(|&&q| idom[q] != usize::MAX) {
                new = if new == usize::MAX {
                    q
                } else {
                    intersect(&idom, q, new)
                };
            }
            if idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
    }
    idom
}

/// Node splitting, before SSA. While a loop is entered at several blocks
/// (a compiler-rotated loop whose peeled first test jumps into its body),
/// the entry first in reverse postorder stays the header, and another
/// entry's outside predecessors move to a copy of everything that entry
/// reaches in the loop before the header. The copies run the same
/// instructions, so behaviour is unchanged.
fn split_irreducible(blocks: &mut Vec<Block>) -> Result<(), ShaderError> {
    let limit = 4 * blocks.len() + 64;
    loop {
        let order = reverse_postorder(blocks);
        let mut index = vec![usize::MAX; blocks.len()];
        for (i, &b) in order.iter().enumerate() {
            index[b] = i;
        }
        let Some((from, _)) = irreducible_edge(blocks, &order, &index) else {
            return Ok(());
        };
        let found = multi_entry_loop(blocks, &vec![true; blocks.len()]);
        let Some((region, mut entries)) = found.filter(|_| blocks.len() < limit) else {
            return Err(ShaderError::Unsupported {
                offset: blocks[from].offset,
                what: "irreducible control flow",
            });
        };
        entries.sort_by_key(|&e| index[e]);
        let (header, entry) = (entries[0], entries[1]);

        // What `entry` reaches in the loop before the header, and its copies.
        let mut copy = vec![usize::MAX; blocks.len()];
        let mut copied = Vec::new();
        let mut stack = vec![entry];
        while let Some(b) = stack.pop() {
            if copy[b] != usize::MAX {
                continue;
            }
            copy[b] = blocks.len() + copied.len();
            copied.push(b);
            for s in blocks[b].exit.successors() {
                if region[s] && s != header {
                    stack.push(s);
                }
            }
        }
        let retarget = |exit: &mut Exit, to: &dyn Fn(usize) -> usize| match exit {
            Exit::Jump(s) => *s = to(*s),
            Exit::Branch { taken, not_taken } => {
                *taken = to(*taken);
                *not_taken = to(*not_taken);
            }
            Exit::End | Exit::Indirect => {}
        };
        for &b in &copied {
            let mut block = blocks[b].clone();
            retarget(&mut block.exit, &|s| {
                if copy[s] == usize::MAX { s } else { copy[s] }
            });
            blocks.push(block);
        }
        let outside: Vec<usize> = blocks[entry]
            .preds
            .iter()
            .copied()
            .filter(|&p| !region[p])
            .collect();
        for p in outside {
            retarget(&mut blocks[p].exit, &|s| {
                if s == entry { copy[entry] } else { s }
            });
        }
        link(blocks);
    }
}

/// A loop among the `inside` blocks entered at more than one block, with
/// its entries. Loops nested in a single-entry loop are searched with its
/// header removed, so one is found whenever the control flow is irreducible.
fn multi_entry_loop(blocks: &[Block], inside: &[bool]) -> Option<(Vec<bool>, Vec<usize>)> {
    let reach = |from: usize, forward: bool| {
        let mut seen = vec![false; blocks.len()];
        let mut stack = vec![from];
        while let Some(b) = stack.pop() {
            let next = if forward {
                blocks[b].exit.successors()
            } else {
                blocks[b].preds.clone()
            };
            for n in next {
                if inside[n] && !seen[n] {
                    seen[n] = true;
                    stack.push(n);
                }
            }
        }
        seen
    };
    let mut done = vec![false; blocks.len()];
    for v in (0..blocks.len()).filter(|&v| inside[v]) {
        if done[v] {
            continue;
        }
        // Blocks on a cycle through `v`; `v` itself only if there is one.
        let (ahead, behind) = (reach(v, true), reach(v, false));
        let region: Vec<bool> = ahead.iter().zip(&behind).map(|(a, b)| a & b).collect();
        if !region[v] {
            continue;
        }
        let members = (0..blocks.len()).filter(|&b| region[b]);
        for b in members.clone() {
            done[b] = true;
        }
        let entries: Vec<usize> = members
            .filter(|&b| blocks[b].preds.iter().any(|&p| !region[p]))
            .collect();
        match entries[..] {
            [] => {}
            [header] => {
                let mut nested = region;
                nested[header] = false;
                if let Some(found) = multi_entry_loop(blocks, &nested) {
                    return Some(found);
                }
            }
            _ => return Some((region, entries)),
        }
    }
    None
}

/// Registers live into and out of each block.
fn liveness(blocks: &[Block], order: &[usize]) -> (Vec<RegSet>, Vec<RegSet>) {
    let empty = [0; REGS.div_ceil(64)];
    let mut uses = vec![empty; blocks.len()];
    let mut defs = vec![empty; blocks.len()];
    for (b, block) in blocks.iter().enumerate() {
        for inst in &block.insts {
            for &(reg, _) in &inst.reads {
                if !contains(&defs[b], reg) {
                    insert(&mut uses[b], reg);
                }
            }
            for &(reg, _) in &inst.writes {
                insert(&mut defs[b], reg);
            }
        }
    }
    let mut live_in = vec![empty; blocks.len()];
    let mut live_out = vec![empty; blocks.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().rev() {
            let mut out = empty;
            for s in blocks[b].exit.successors() {
                for (o, i) in out.iter_mut().zip(&live_in[s]) {
                    *o |= i;
                }
            }
            let mut inn = empty;
            for w in 0..inn.len() {
                inn[w] = uses[b][w] | out[w] & !defs[b][w];
            }
            if inn != live_in[b] || out != live_out[b] {
                (live_in[b], live_out[b]) = (inn, out);
                changed = true;
            }
        }
    }
    (live_in, live_out)
}

/// Phis are placed for every live register at every join; those whose
/// arguments are all one value (or the phi itself) are replaced by it.
fn remove_trivial_phis(blocks: &mut [Block], values: u32) {
    remove_trivial_phis_steps(blocks, values);
}

fn remove_trivial_phis_steps(blocks: &mut [Block], values: u32) -> usize {
    fn find(replaced: &mut [Value], v: Value, steps: &mut usize) -> Value {
        let mut root = v;
        while replaced[root.0 as usize] != root {
            *steps += 1;
            root = replaced[root.0 as usize];
        }
        let mut cur = v;
        while replaced[cur.0 as usize] != root {
            *steps += 1;
            let next = replaced[cur.0 as usize];
            replaced[cur.0 as usize] = root;
            cur = next;
        }
        root
    }

    let mut replaced: Vec<Value> = (0..values).map(Value).collect();
    let mut steps = 0usize;
    let mut changed = true;
    while changed {
        changed = false;
        for block in blocks.iter_mut() {
            block.phis.retain(|phi| {
                let mut only = None;
                for &a in &phi.args {
                    let a = find(&mut replaced, a, &mut steps);
                    if a == phi.value || Some(a) == only {
                        continue;
                    }
                    if only.is_some() {
                        return true;
                    }
                    only = Some(a);
                }
                let Some(v) = only else { return true };
                replaced[phi.value.0 as usize] = v;
                changed = true;
                false
            });
        }
    }
    for block in blocks {
        for phi in &mut block.phis {
            for a in &mut phi.args {
                *a = find(&mut replaced, *a, &mut steps);
            }
        }
        for inst in &mut block.insts {
            for (_, v) in &mut inst.reads {
                *v = find(&mut replaced, *v, &mut steps);
            }
        }
    }
    steps
}

/// Scheduling hints, performance counters and tracing: no effect on results.
const DROPPED: &[&str] = &[
    "s_nop",
    "s_waitcnt",
    "s_waitcnt_vscnt",
    "s_waitcnt_vmcnt",
    "s_waitcnt_expcnt",
    "s_waitcnt_lgkmcnt",
    "s_waitcnt_depctr",
    "s_wait_idle",
    "s_inst_prefetch",
    "s_clause",
    "s_setprio",
    "s_sleep",
    "s_ttracedata",
    "s_ttracedata_imm",
    "s_incperflevel",
    "s_decperflevel",
    "v_nop",
    "v_pipeflush",
    "v_clrexcp",
];

/// Registers read, registers written.
type Effects = (Vec<Reg>, Vec<Reg>);

/// The registers `inst` of `p` reads as operands: its reads without the old
/// value of a VGPR it only writes.
pub(crate) fn operand_reads(p: &Program, inst: &Inst) -> Vec<Reg> {
    let lanes = 1 + u8::from(p.wave64);
    effects(&inst.machine, &inst.fields, inst.name(), lanes, p.vgprs, None)
        .ok()
        .flatten()
        .map_or_else(Vec::new, |e| e.0)
}

/// Registers an instruction reads and writes, one per dword, or `None` if
/// the IR drops it.
fn effects(
    inst: &Instruction,
    fields: &Fields,
    name: &'static str,
    lanes: u8,
    vgprs: u16,
    m0_max: Option<u8>,
) -> Result<Option<Effects>, ShaderError> {
    if DROPPED.contains(&name) {
        return Ok(None);
    }
    let unsupported = || ShaderError::Unsupported {
        offset: inst.offset,
        what: name,
    };
    let mask = |code: u8| (0..lanes).map(move |i| Reg::S(code + i));
    let (mut r, mut w) = (Vec::new(), Vec::new());
    let regs = |op: &Operand, out: &mut Vec<Reg>| {
        add_regs(op, lanes, out).map_err(|_| ShaderError::Isa {
            offset: inst.offset,
            reason: "register range out of bounds",
        })
    };
    let vector = !matches!(
        fields,
        Fields::Sop { .. } | Fields::Sopp { .. } | Fields::Smem { .. }
    );
    match fields {
        Fields::Sop { dst, src, .. } => {
            if name.starts_with("s_movrel")
                || name.starts_with("s_subvector_loop")
                || matches!(name, "s_call_b64" | "s_rfe_b64")
            {
                return Err(unsupported());
            }
            for s in src {
                regs(s, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
                // These change (or may keep) the destination's old value.
                if name.starts_with("s_bitset")
                    || name.starts_with("s_cmov")
                    || matches!(name, "s_addk_i32" | "s_mulk_i32")
                {
                    regs(d, &mut r)?;
                }
            }
            if matches!(name, "s_addc_u32" | "s_subb_u32" | "s_cmovk_i32")
                || name.starts_with("s_cselect_")
                || name.starts_with("s_cmov_")
            {
                r.push(Reg::Scc);
            }
            if name.contains("saveexec") || name.contains("wrexec") {
                r.extend(mask(EXEC));
                w.extend(mask(EXEC));
            }
            if writes_scc(inst.encoding, name) {
                w.push(Reg::Scc);
            }
        }
        Fields::Sopp { .. } => match name {
            "s_cbranch_scc0" | "s_cbranch_scc1" => r.push(Reg::Scc),
            "s_cbranch_vccz" | "s_cbranch_vccnz" => r.extend(mask(VCC)),
            "s_cbranch_execz" | "s_cbranch_execnz" => r.extend(mask(EXEC)),
            "s_sendmsg" | "s_sendmsghalt" => r.push(Reg::S(M0)),
            "s_endpgm"
            | "s_endpgm_saved"
            | "s_endpgm_ordered_ps_done"
            | "s_code_end"
            | "s_branch"
            | "s_barrier"
            | "s_icache_inv"
            | "s_wakeup"
            | "s_round_mode"
            | "s_denorm_mode" => {}
            _ => return Err(unsupported()),
        },
        Fields::Smem {
            data,
            base,
            soffset,
            ..
        } => match name {
            "s_memtime" | "s_memrealtime" | "s_get_waveid_in_workgroup" => {
                regs(data.as_ref().ok_or_else(unsupported)?, &mut w)?;
            }
            "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb" => {}
            _ if name.contains("_load_") || name.contains("_store_") => {
                regs(base, &mut r)?;
                regs(soffset, &mut r)?;
                let data = data.as_ref().ok_or_else(unsupported)?;
                regs(
                    data,
                    if name.contains("_load_") {
                        &mut w
                    } else {
                        &mut r
                    },
                )?;
            }
            _ => return Err(unsupported()),
        },
        Fields::Valu(v) => {
            let (src_indexed, dst_indexed) = match name {
                "v_movrels_b32" => (true, false),
                "v_movreld_b32" => (false, true),
                "v_movrelsd_b32" | "v_movrelsd_2_b32" | "v_swaprel_b32" => (true, true),
                _ => (false, false),
            };
            // v[reg + M0] inside the program's VGPRs and a proven bound.
            let indexed = |op: &Operand, out: &mut Vec<Reg>| match *op {
                Operand::Vector { reg, .. } if u16::from(reg) < vgprs => {
                    let end = m0_max.map_or(vgprs, |m| vgprs.min(u16::from(reg) + u16::from(m) + 1));
                    out.extend((u16::from(reg)..end).map(|r| Reg::V(r as u8)));
                    Ok(())
                }
                _ => Err(ShaderError::Isa {
                    offset: inst.offset,
                    reason: "relative VGPR base outside the program's VGPRs",
                }),
            };
            for (i, s) in v.src.iter().enumerate() {
                if i == 0 && src_indexed {
                    indexed(s, &mut r)?;
                } else {
                    regs(s, &mut r)?;
                }
            }
            // GFX10 compares into EXEC write nothing else.
            if name.starts_with("v_cmpx") {
                w.extend(mask(EXEC));
            } else if let Some(d) = &v.dst {
                if dst_indexed {
                    indexed(d, &mut w)?;
                } else {
                    regs(d, &mut w)?;
                }
            }
            if let Some(c) = &v.carry {
                regs(c, &mut w)?;
            }
            match name {
                "v_swap_b32" => {
                    regs(&v.src[0], &mut w)?;
                    if let Some(d) = &v.dst {
                        regs(d, &mut r)?;
                    }
                }
                "v_swaprel_b32" => {
                    indexed(&v.src[0], &mut w)?;
                    if let Some(d) = &v.dst {
                        indexed(d, &mut r)?;
                    }
                }
                "v_div_fmas_f32" | "v_div_fmas_f64" => r.extend(mask(VCC)),
                _ => {}
            }
            if name.starts_with("v_interp_") || src_indexed || dst_indexed {
                r.push(Reg::S(M0));
            }
        }
        Fields::Vintrp { dst, src, .. } => {
            regs(src, &mut r)?;
            r.push(Reg::S(M0));
            regs(dst, &mut w)?;
        }
        Fields::Ds {
            dst,
            addr,
            data,
            gds,
            ..
        } => {
            for o in addr.iter().chain(data) {
                regs(o, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
            }
            if *gds
                || name.starts_with("ds_gws")
                || name.contains("addtid")
                || matches!(name, "ds_ordered_count" | "ds_append" | "ds_consume")
            {
                r.push(Reg::S(M0));
            }
        }
        Fields::Buffer {
            data,
            addr,
            rsrc,
            soffset,
            offen,
            idxen,
            lds,
            cache,
            ..
        } => {
            if *offen || *idxen {
                regs(addr, &mut r)?;
            }
            regs(rsrc, &mut r)?;
            regs(soffset, &mut r)?;
            if *lds {
                r.push(Reg::S(M0));
            }
            if let Some(d) = data {
                if name.contains("_load_") {
                    if !lds {
                        regs(d, &mut w)?;
                    }
                } else if name.contains("_store_") {
                    regs(d, &mut r)?;
                } else if name.contains("_atomic_") {
                    regs(d, &mut r)?;
                    if cache.glc {
                        regs(&atomic_result(d, name), &mut w)?;
                    }
                } else {
                    return Err(unsupported());
                }
            }
        }
        Fields::Image {
            data,
            addr,
            rsrc,
            sampler,
            cache,
            ..
        } => {
            for a in addr.iter().chain([rsrc]).chain(sampler) {
                regs(a, &mut r)?;
            }
            if name.contains("_store") {
                regs(data, &mut r)?;
            } else if name.contains("_atomic_") {
                regs(data, &mut r)?;
                if cache.glc {
                    regs(&atomic_result(data, name), &mut w)?;
                }
            } else {
                regs(data, &mut w)?;
            }
        }
        Fields::Flat {
            dst,
            addr,
            data,
            saddr,
            ..
        } => {
            for o in [addr, data, saddr].into_iter().flatten() {
                regs(o, &mut r)?;
            }
            if let Some(d) = dst {
                regs(d, &mut w)?;
            }
        }
        Fields::Exp { src, .. } => {
            for s in src.iter().flatten() {
                regs(s, &mut r)?;
            }
        }
    }
    if vector {
        r.extend(mask(EXEC));
    }
    let mut seen = Vec::with_capacity(r.len());
    r.retain(|reg| {
        let new = !seen.contains(reg);
        seen.push(*reg);
        new
    });
    Ok(Some((r, w)))
}

/// Scalar ALU opcodes that set SCC (carry, overflow, comparison, or "result
/// is not zero").
fn writes_scc(encoding: gfx10::Encoding, name: &str) -> bool {
    use gfx10::Encoding::*;
    match encoding {
        Sopc => true,
        Sopk => name.starts_with("s_cmpk_") || name == "s_addk_i32",
        _ => {
            let stem = name.split('_').nth(1).unwrap_or("");
            name.contains("saveexec")
                || name.contains("wrexec")
                || matches!(
                    stem,
                    "add"
                        | "sub"
                        | "addc"
                        | "subb"
                        | "min"
                        | "max"
                        | "and"
                        | "or"
                        | "xor"
                        | "andn2"
                        | "orn2"
                        | "nand"
                        | "nor"
                        | "xnor"
                        | "lshl"
                        | "lshr"
                        | "ashr"
                        | "bfe"
                        | "absdiff"
                        | "lshl1"
                        | "lshl2"
                        | "lshl3"
                        | "lshl4"
                        | "not"
                        | "wqm"
                        | "bcnt0"
                        | "bcnt1"
                        | "quadmask"
                        | "abs"
                )
        }
    }
}

/// What an atomic with `glc` returns: the old value (for compare-and-swap,
/// half the data operand).
fn atomic_result(data: &Operand, name: &str) -> Operand {
    match *data {
        Operand::Vector { reg, dwords } if name.contains("cmpswap") => Operand::Vector {
            reg,
            dwords: dwords / 2,
        },
        op => op,
    }
}

/// Appends the register dwords an operand names; reading `vccz`, `execz`,
/// `scc` or `lds_direct` reads the register behind it.
fn add_regs(op: &Operand, lanes: u8, out: &mut Vec<Reg>) -> Result<(), ()> {
    match *op {
        Operand::Scalar { code, .. } if code == gfx10::NULL => {}
        Operand::Scalar { code, dwords } => {
            let end = u16::from(code) + u16::from(dwords);
            // A machine operand ends within the machine's codes, a parked
            // scalar (`unspill`) within those past them.
            let limit = if code < PARKED { 128 } else { 256 };
            if end > limit {
                return Err(());
            }
            out.extend((u16::from(code)..end).map(|c| Reg::S(c as u8)));
        }
        Operand::Vector { reg, dwords } => {
            if u16::from(reg) + u16::from(dwords) > 256 {
                return Err(());
            }
            out.extend((0..dwords).map(|i| Reg::V(reg + i)));
        }
        Operand::Const(251) => out.extend((0..lanes).map(|i| Reg::S(VCC + i))),
        Operand::Const(252) => out.extend((0..lanes).map(|i| Reg::S(EXEC + i))),
        Operand::Const(253) => out.push(Reg::Scc),
        Operand::Const(254) => out.push(Reg::S(M0)),
        Operand::Const(_) | Operand::Literal(_) | Operand::Attr { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn build_words(words: &[u32], wave64: bool) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        build(&code, wave64, 256).unwrap()
    }

    fn input(p: &Program, reg: Reg) -> Value {
        p.inputs.iter().find(|i| i.0 == reg).expect("input").1
    }

    /// `if (v0 < s0) { ... }` whose body changes s6: `body` are its words, `after` what
    /// follows the join.
    fn skip(body: &[u32], after: &[u32]) -> Program {
        let mut w = vec![
            0xBE94_0306,                     // s_mov_b32 s20, s6
            0x7D88_0000,                     // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A,                     // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0000 | body.len() as u32, // s_cbranch_execz past the body
        ];
        w.extend_from_slice(body);
        w.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
        w.extend_from_slice(after);
        w.push(S_ENDPGM);
        build_words(&w, true)
    }

    fn skips(p: &Program) -> usize {
        p.blocks
            .iter()
            .filter(|b| b.insts.last().is_some_and(|i| i.name() == "s_cbranch_execz"))
            .count()
    }

    /// `if (v4 >= s9) return;` (`v_cmpx_gt_u32 exec, s9, v4`, then a branch to
    /// the end when no lane is left), then `body`.
    fn early_out(body: &[u32]) -> Program {
        let mut w = vec![0x7DA8_0809, 0xBF88_0000 | body.len() as u32];
        w.extend_from_slice(body);
        w.push(S_ENDPGM);
        build_words(&w, true)
    }

    #[test]
    fn a_skip_is_left_out_when_the_scalars_it_changes_come_back_or_are_not_read() {
        const SET: u32 = 0xBE86_0385; // s_mov_b32 s6, 5
        const USE: u32 = 0x7E02_0206; // v_mov_b32 v1, s6
        const BACK: u32 = 0xBE86_0314; // s_mov_b32 s6, s20
        const READ: u32 = 0x7E04_0206; // v_mov_b32 v2, s6
        // s6 is changed in the body and read after the join: the branch stays
        let kept = skip(&[SET, USE], &[READ]);
        assert_eq!(skips(&kept), 1);
        assert!(without_skips(&kept, false).is_none());
        assert!(without_skips(&kept, true).is_none());
        // ... put back by a copy of what it held (as a scalar parked in a VGPR lane comes
        // back): every lane may walk the body
        let back = skip(&[SET, USE, BACK], &[READ]);
        assert!(without_skips(&back, false).is_none());
        assert_eq!(skips(&without_skips(&back, true).expect("left out")), 0);
        // ... or not read after the join at all
        let unread = skip(&[SET, USE], &[]);
        assert_eq!(skips(&without_skips(&unread, false).expect("left out")), 0);
        assert_eq!(skips(&without_skips(&unread, true).expect("left out")), 0);
        // ... or read only by a restore of EXEC that nothing uses before the next one
        // (s_mov_b64 exec, s[6:7]; s_mov_b64 exec, s[12:13]; v_mov_b32 v2, s0)
        let idle = skip(&[SET, USE], &[0xBEFE_0406, 0xBEFE_040C, 0x7E04_0200]);
        assert!(without_skips(&idle, false).is_none());
        assert_eq!(skips(&without_skips(&idle, true).expect("left out")), 0);
    }

    #[test]
    fn an_empty_exec_skip_keeps_a_store_behind_a_mask_restore() {
        // A comparison rejected all lanes, so the console skips the store. If
        // the branch disappears, restoring the saved incoming EXEC runs it.
        const RESTORE: u32 = 0xBEFE_040C; // s_mov_b64 exec, s[12:13]
        const STORE: [u32; 2] = [0xD834_0000, 0x0000_0C00]; // ds_write_b32 v0, v12
        let kept = skip(&[RESTORE, STORE[0], STORE[1]], &[]);
        assert!(without_skips(&kept, false).is_none());
        assert!(without_skips(&kept, true).is_none());
        let early = build_words(
            &[0x7D88_0000, 0xBE8C_246A, 0xBF88_0003, RESTORE, STORE[0], STORE[1], S_ENDPGM],
            true,
        );
        assert!(without_early_outs(&early).is_none());
        assert!(without_skips(&early, true).is_none());
        // With no restore the store stays masked and the branch can go.
        assert_eq!(skips(&without_skips(&skip(&STORE, &[]), true).unwrap()), 0);
        // A nested save of the already-empty EXEC, restored from precisely
        // that definition before the store, cannot enable any lanes.
        let nested = skip(&[0xBE90_246A, 0xBEFE_0410, STORE[0], STORE[1]], &[]);
        assert_eq!(skips(&without_skips(&nested, true).unwrap()), 0);
        // OR-save also activates lanes if its operand was saved before
        // the skip, even though it saves the (empty) current EXEC.
        let activates = skip(&[0xBE90_250C, STORE[0], STORE[1]], &[]);
        assert!(without_skips(&activates, true).is_none());
    }

    /// The wave sum the console compiler puts behind a bounds test
    /// (b22a16a178976e1b, a matrix transpose-multiply program): EXEC opened
    /// to the whole wave, a masked input summed, and EXEC put back from the
    /// mask saved while it was clear. Only registers change in that window, so
    /// the early-out can go. A store inside it, or a window still open where
    /// its block ends, keeps the branch.
    #[test]
    fn a_whole_wave_window_that_only_writes_registers_lets_an_early_out_go() {
        const OPEN: u32 = 0xBE84_25C1; // s_or_saveexec_b64 s[4:5], -1
        const SUM: [u32; 4] = [
            0xD501_0001,
            0x0012_0E80, // v_cndmask_b32 v1, 0, v7, s[4:5]
            0xD760_0007,
            0x0001_0101, // v_readlane_b32 s7, v1, 0
        ];
        const CLOSE: u32 = 0xBEFE_0404; // s_mov_b64 exec, s[4:5]
        const STORE: [u32; 2] = [0xD834_0000, 0x0000_0C00]; // ds_write_b32 v0, v12
        let p = early_out(&[&[OPEN][..], &SUM[..], &[CLOSE][..], &STORE[..]].concat());
        assert_eq!(skips(&without_early_outs(&p).expect("left out")), 0);
        assert_eq!(skips(&without_skips(&p, true).expect("left out")), 0);
        let inside = early_out(&[&[OPEN][..], &SUM[..], &STORE[..], &[CLOSE][..]].concat());
        assert!(without_early_outs(&inside).is_none());
        assert!(without_skips(&inside, true).is_none());
        // (s_cmp_eq_u32 s0, s1; s_cbranch_scc1 past the store)
        let across = early_out(
            &[
                &[OPEN][..],
                &SUM[..2],
                &[0xBF06_0100, 0xBF85_0002][..],
                &STORE[..],
                &[CLOSE][..],
            ]
            .concat(),
        );
        assert!(without_early_outs(&across).is_none());
        assert!(without_skips(&across, true).is_none());
        // A mask saved inside the window is the whole wave: EXEC taken from it
        // after the window runs the store (s_or_saveexec_b64 s[2:3], -1 inside;
        // s_mov_b64 exec, s[2:3] after).
        let saved = early_out(
            &[
                &[OPEN, 0xBE82_25C1, CLOSE, 0xBEFE_0402][..],
                &STORE[..],
                &[CLOSE][..],
            ]
            .concat(),
        );
        assert!(without_early_outs(&saved).is_none());
    }

    /// Masks made from the clear EXEC of a skipped region are clear where the
    /// operation keeps zero whatever the other mask (AND, ANDN2 of EXEC), and
    /// not where it keeps the other mask's lanes (OR, ANDN2 from it).
    #[test]
    fn a_mask_from_clear_exec_is_clear_only_where_its_operation_keeps_zero() {
        const STORE: [u32; 2] = [0xD834_0000, 0x0000_0C00]; // ds_write_b32 v0, v12
        // s_and_saveexec_b64 s[16:17], s[12:13] (the mask from before the branch), the
        // store, s_mov_b64 exec, s[16:17]
        let and = skip(&[0xBE90_240C, STORE[0], STORE[1], 0xBEFE_0410], &[]);
        assert_eq!(skips(&without_skips(&and, true).expect("left out")), 0);
        // s_andn2_b64 exec, exec, s[12:13]
        let and_not = skip(&[0x8AFE_0C7E, STORE[0], STORE[1]], &[]);
        assert_eq!(skips(&without_skips(&and_not, true).expect("left out")), 0);
        // s_or_saveexec_b64 s[16:17], s[12:13]; s_andn2_b64 exec, s[12:13], exec
        for opens in [0xBE90_250C, 0x8AFE_7E0C] {
            let p = skip(&[opens, STORE[0], STORE[1]], &[]);
            assert!(without_skips(&p, true).is_none(), "{opens:#x}");
        }
    }

    /// An if/else behind an early-out as the compiler writes it: every mask
    /// is a save, a combination or a restore of the clear EXEC the lanes that
    /// left walk with, so all of them stay clear.
    #[test]
    fn an_if_else_behind_an_early_out_keeps_exec_clear() {
        let p = early_out(&[
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE82_246A, // s_and_saveexec_b64 s[2:3], vcc
            0x8982_027E, // s_xor_b64 s[2:3], exec, s[2:3]
            0xBF88_0001, // s_cbranch_execz -> else
            0x7E02_0287, // v_mov_b32 v1, 7
            0xBE82_2502, // else: s_or_saveexec_b64 s[2:3], s[2:3]
            0x89FE_027E, // s_xor_b64 exec, exec, s[2:3]
            0xBF88_0001, // s_cbranch_execz -> endif
            0x7E02_0289, // v_mov_b32 v1, 9
            0x88FE_027E, // endif: s_or_b64 exec, exec, s[2:3]
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
        ]);
        // The early-out goes; the skips of the then and the else stay.
        assert_eq!(skips(&without_early_outs(&p).expect("left out")), 2);
        assert_eq!(skips(&without_skips(&p, true).expect("left out")), 0);
    }

    /// A loop whose lanes leave one by one (the mask of those that left grows,
    /// EXEC loses it, the restore after the loop puts both together): entered
    /// with EXEC clear, every mask in it stays clear, which shows only when the
    /// loop's joins are taken as clear until a way in brings something else.
    #[test]
    fn a_divergent_loop_behind_an_early_out_keeps_exec_clear() {
        let p = early_out(&[
            0xBE86_0480, // s_mov_b64 s[6:7], 0
            0x7D88_0000, // loop: v_cmp_gt_u32 vcc, s0, v0
            0x8886_066A, // s_or_b64 s[6:7], vcc, s[6:7]
            0x8AFE_067E, // s_andn2_b64 exec, exec, s[6:7]
            0xBF89_FFFC, // s_cbranch_execnz -> loop
            0x88FE_067E, // s_or_b64 exec, exec, s[6:7]
            0xD834_0000,
            0x0000_0100, // ds_write_b32 v0, v1
        ]);
        assert_eq!(skips(&without_early_outs(&p).expect("left out")), 0);
        // A mask saved before the branch that comes back around the loop opens
        // EXEC on the second trip, and the store runs.
        let back = build_words(
            &[
                0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
                0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
                0xBF88_0008, // s_cbranch_execz -> s_endpgm
                0xBE86_0480, // s_mov_b64 s[6:7], 0
                0x88FE_067E, // loop: s_or_b64 exec, exec, s[6:7]
                0xD834_0000,
                0x0000_0100, // ds_write_b32 v0, v1
                0xBE86_040C, // s_mov_b64 s[6:7], s[12:13]
                0xBEFE_0480, // s_mov_b64 exec, 0
                0xBF06_0100, // s_cmp_eq_u32 s0, s1
                0xBF85_FFF9, // s_cbranch_scc1 -> loop
                S_ENDPGM,
            ],
            true,
        );
        assert!(without_early_outs(&back).is_none());
        assert!(without_skips(&back, true).is_none());
    }

    /// A compare where EXEC is clear writes a clear mask (lanes EXEC leaves
    /// out get 0), so an EXEC made from it is clear too; the same compare
    /// inside a whole-wave window sees every lane. A compare into EXEC keeps
    /// EXEC as clear as it was, and the window open.
    #[test]
    fn a_compare_writes_a_clear_mask_only_where_exec_is_clear() {
        const CMP: u32 = 0x7D88_0000; // v_cmp_gt_u32 vcc, s0, v0
        const SAVE: u32 = 0xBE82_256A; // s_or_saveexec_b64 s[2:3], vcc
        const STORE: [u32; 2] = [0xD834_0000, 0x0000_0100]; // ds_write_b32 v0, v1
        const RESTORE: u32 = 0xBEFE_0402; // s_mov_b64 exec, s[2:3]
        const OPEN: u32 = 0xBE84_25C1; // s_or_saveexec_b64 s[4:5], -1
        const CLOSE: u32 = 0xBEFE_0404; // s_mov_b64 exec, s[4:5]
        const CMPX: u32 = 0x7DA8_0809; // v_cmpx_gt_u32 exec, s9, v4
        let clear = early_out(&[CMP, SAVE, STORE[0], STORE[1], RESTORE]);
        assert_eq!(skips(&without_early_outs(&clear).expect("left out")), 0);
        let whole = early_out(&[OPEN, CMP, CLOSE, SAVE, STORE[0], STORE[1], RESTORE]);
        assert!(without_early_outs(&whole).is_none());
        let into_exec = early_out(&[CMPX, STORE[0], STORE[1]]);
        assert_eq!(skips(&without_early_outs(&into_exec).expect("left out")), 0);
        let in_window = early_out(&[OPEN, CMPX, STORE[0], STORE[1], CLOSE]);
        assert!(without_early_outs(&in_window).is_none());
    }

    /// A window that is not the whole wave keeps the branch. Inside
    /// `if (t < s0)` the region puts back the mask from before the branch,
    /// writes v1 and clears EXEC again with the mask it saved while EXEC was
    /// clear; the join stores v1. With no thread below s0 the console skips
    /// the region and stores 7, and lanes walking it would store 9.
    #[test]
    fn a_window_of_a_mask_from_before_the_branch_keeps_the_branch() {
        let window = |open: u32, close: u32| {
            build_words(
                &[
                    0x7E02_0287, // v_mov_b32 v1, 7
                    0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
                    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
                    0xBF88_0004, // s_cbranch_execz -> join
                    0xBE8E_047E, // s_mov_b64 s[14:15], exec (clear when skipped)
                    open,
                    0x7E02_0289, // v_mov_b32 v1, 9
                    close,
                    0xBEFE_040C, // join: s_mov_b64 exec, s[12:13]
                    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
                    0xE070_1000,
                    0x8002_0104, // buffer_store_dword v1, v4, s[8:11], 0 offen
                    S_ENDPGM,
                ],
                true,
            )
        };
        // s_mov_b64 exec, s[12:13] (the lanes from before the branch) ...
        // s_mov_b64 exec, s[14:15]
        let before = window(0xBEFE_040C, 0xBEFE_040E);
        assert_eq!(skips(&before), 1);
        assert!(without_skips(&before, false).is_none());
        assert!(without_skips(&before, true).is_none());
        // A whole-wave opening put back by a mask it did not save: s_or_saveexec_b64
        // s[16:17], -1 ... s_mov_b64 exec, s[14:15] (clear too, but not that window's).
        let other = window(0xBE90_25C1, 0xBEFE_040E);
        assert!(without_skips(&other, true).is_none());
    }

    /// A prefix sum over the wave narrows its whole-wave window with constants
    /// (bad01f53, a material tile setup program: rows 1 and 3, then every lane
    /// again). That still writes registers only, so the early-out can go. A
    /// mask from a register or from SCC there keeps it.
    #[test]
    fn a_whole_wave_window_may_narrow_to_fixed_masks() {
        const OPEN: u32 = 0xBE84_25C1; // s_or_saveexec_b64 s[4:5], -1
        const CLOSE: u32 = 0xBEFE_0404; // s_mov_b64 exec, s[4:5]
        const STORE: [u32; 2] = [0xD834_0000, 0x0000_0C00]; // ds_write_b32 v0, v12
        let window = |narrow: &[u32]| {
            early_out(
                &[
                    &[OPEN, 0xD501_0001, 0x0012_0E80][..], // v_cndmask_b32 v1, 0, v7, s[4:5]
                    narrow,
                    &[0x7E04_0301, CLOSE][..], // v_mov_b32 v2, v1
                    &STORE[..],
                ]
                .concat(),
            )
        };
        let rows = window(&[
            0xBEFE_03FF,
            0xFFFF_0000, // s_mov_b32 exec_lo, 0xffff0000
            0xBEFF_03FF,
            0xFFFF_0000, // s_mov_b32 exec_hi, 0xffff0000
            0x7E04_0301, // v_mov_b32 v2, v1
            0xBEFE_03C1, // s_mov_b32 exec_lo, -1
            0xBEFF_03C1, // s_mov_b32 exec_hi, -1
        ]);
        assert_eq!(skips(&without_early_outs(&rows).expect("left out")), 0);
        assert_eq!(skips(&without_skips(&rows, true).expect("left out")), 0);
        // s_mov_b64 exec, s[2:3]; s_mov_b32 exec_lo, scc
        for narrow in [0xBEFE_0402, 0xBEFE_03FD] {
            let p = window(&[narrow]);
            assert!(without_early_outs(&p).is_none(), "{narrow:#x}");
            assert!(without_skips(&p, true).is_none(), "{narrow:#x}");
        }
    }

    /// A loop that goes on "while no running lane wants out" behind an
    /// early-out (c4183d3c, a projected-decal resolve program). EXEC stays clear
    /// in it, but a host lane decides `s_cbranch_vccz` by its own bit: outside
    /// EXEC it never leaves, counts to the lowering's limit and runs the store
    /// after the loop. The early-out stays; with a way out on a scalar the
    /// lane leaves with its wave and the early-out can go.
    #[test]
    fn a_loop_no_lane_outside_exec_leaves_keeps_the_early_out() {
        const TAIL: [u32; 4] = [
            0x7E06_0214, // v_mov_b32 v3, s20
            0x3408_0082, // v_lshlrev_b32 v4, 2, v0
            0xE070_1000,
            0x8002_0304, // buffer_store_dword v3, v4, s[8:11], 0 offen
        ];
        let behind = |lp: &[u32]| {
            let mut words = vec![
                0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
                0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
                0xBF88_0000 | (1 + lp.len() + TAIL.len()) as u32, // s_cbranch_execz -> s_endpgm
                0xBE94_0380, // s_mov_b32 s20, 0
            ];
            words.extend_from_slice(lp);
            words.extend_from_slice(&TAIL);
            words.push(S_ENDPGM);
            build_words(&words, true)
        };
        let spins = behind(&[
            0x8114_8114, // loop: s_add_i32 s20, s20, 1
            0x7E04_0214, // v_mov_b32 v2, s20
            0x7D88_0401, // v_cmp_gt_u32 vcc, s1, v2 (go on while s1 > count)
            0x8AEA_6A7E, // s_andn2_b64 vcc, exec, vcc (running lanes that want out)
            0xBF86_FFFB, // s_cbranch_vccz -> loop
        ]);
        assert!(without_early_outs(&spins).is_none());
        assert!(without_skips(&spins, true).is_none());
        let counted = behind(&[
            0x8114_8114, // loop: s_add_i32 s20, s20, 1
            0xBF0A_0114, // s_cmp_lt_u32 s20, s1
            0xBF85_FFFD, // s_cbranch_scc1 -> loop
        ]);
        assert_eq!(skips(&without_early_outs(&counted).expect("left out")), 0);
        // The SCC of a mask operation on the clear EXEC is as decided as the
        // mask: s_andn2_b64 s[6:7], exec, vcc; s_cbranch_scc0 -> loop.
        let on_scc = behind(&[
            0x8114_8114, // loop: s_add_i32 s20, s20, 1
            0x8A86_6A7E, // s_andn2_b64 s[6:7], exec, vcc
            0xBF84_FFFD, // s_cbranch_scc0 -> loop
        ]);
        assert!(without_early_outs(&on_scc).is_none());
    }

    #[test]
    fn joins_get_phis_only_where_values_differ() {
        let code = [
            0xBF06_0100, // s_cmp_eq_u32 s0, s1
            0xBF85_0001, // s_cbranch_scc1 -> 12
            0xBE82_0380, // s_mov_b32 s2, 0
            0x7E00_0202, // 12: v_mov_b32 v0, s2
            0x7E02_0200, // v_mov_b32 v1, s0
            S_ENDPGM,
        ];
        for wave64 in [false, true] {
            let p = build_words(&code, wave64);
            let [b0, b1, b2] = &p.blocks[..] else {
                panic!("{:?}", p.blocks)
            };
            assert_eq!(
                b0.exit,
                Exit::Branch {
                    taken: 2,
                    not_taken: 1
                }
            );
            assert_eq!(b1.exit, Exit::Jump(2));
            assert_eq!(b2.preds, [0, 1]);
            assert_eq!(b0.insts[1].read(Reg::Scc), b0.insts[0].write(Reg::Scc));

            // s2 differs per path; s0, EXEC, v0 and v1 do not.
            let [phi] = &b2.phis[..] else {
                panic!("{:?}", b2.phis)
            };
            assert_eq!(phi.reg, Reg::S(2));
            assert_eq!(
                phi.args,
                [input(&p, Reg::S(2)), b1.insts[0].write(Reg::S(2)).unwrap()]
            );
            let [mov0, mov1, end] = &b2.insts[..] else {
                panic!()
            };
            assert_eq!(mov0.read(Reg::S(2)), Some(phi.value));
            assert_eq!(mov1.read(Reg::S(0)), Some(input(&p, Reg::S(0))));
            // The old v1 is read: lanes outside EXEC keep it.
            assert_eq!(mov1.read(Reg::V(1)), Some(input(&p, Reg::V(1))));
            assert_eq!(mov1.read(Reg::S(EXEC)), Some(input(&p, Reg::S(EXEC))));
            assert_eq!(mov1.read(Reg::S(EXEC + 1)).is_some(), wave64);
            assert_eq!((end.name(), b2.exit), ("s_endpgm", Exit::End));
        }
    }

    #[test]
    fn in_place_scalar_ops_read_their_destination() {
        let p = build_words(
            &[
                0xBE8D_1D94, // s_bitset1_b32 s13, 20
                0xB78E_0010, // s_addk_i32 s14, 0x10
                0xBE8F_0380, // s_mov_b32 s15, 0
                S_ENDPGM,
            ],
            false,
        );
        let [bitset, addk, mov, _] = &p.blocks[0].insts[..] else {
            panic!("{:?}", p.blocks[0].insts)
        };
        assert_eq!(bitset.read(Reg::S(13)), Some(input(&p, Reg::S(13))));
        assert_eq!(addk.read(Reg::S(14)), Some(input(&p, Reg::S(14))));
        assert_eq!(mov.read(Reg::S(15)), None);
    }

    #[test]
    fn loops_carry_values_through_phis() {
        let p = build_words(
            &[
                0xBE80_0380, // s_mov_b32 s0, 0
                0x8000_8100, // 4: s_add_u32 s0, s0, 1
                0xBF07_8400, // s_cmp_lg_u32 s0, 4
                0xBF85_FFFD, // s_cbranch_scc1 -> 4
                S_ENDPGM,
            ],
            true,
        );
        assert!(p.inputs.is_empty(), "{:?}", p.inputs);
        let [b0, b1, b2] = &p.blocks[..] else {
            panic!()
        };
        assert_eq!(b1.preds, [0, 1]);
        assert_eq!(
            b1.exit,
            Exit::Branch {
                taken: 1,
                not_taken: 2
            }
        );
        let [phi] = &b1.phis[..] else {
            panic!("{:?}", b1.phis)
        };
        let add = &b1.insts[0];
        assert_eq!(phi.reg, Reg::S(0));
        assert_eq!(
            phi.args,
            [
                b0.insts[0].write(Reg::S(0)).unwrap(),
                add.write(Reg::S(0)).unwrap()
            ]
        );
        assert_eq!(add.read(Reg::S(0)), Some(phi.value));
        assert_eq!(b1.insts[1].read(Reg::S(0)), add.write(Reg::S(0)));
        assert_eq!(b2.insts[0].name(), "s_endpgm");
    }

    #[test]
    fn drops_scheduling_and_follows_far_jumps() {
        let p = build_words(
            &[
                0xBF80_0000, // s_nop 0
                0xBE88_1F00, // s_getpc_b64 s[8:9]
                0x8008_FF08,
                20,          // s_add_u32 s8, s8, 20  -> 8 + 20 = 28
                0x8209_8009, // s_addc_u32 s9, s9, 0
                0xBE80_2008, // s_setpc_b64 s[8:9]
                0xFFFF_FFFF, // not code
                S_ENDPGM,
            ],
            false,
        );
        let names: Vec<_> = p.blocks[0].insts.iter().map(Inst::name).collect();
        assert_eq!(
            names,
            ["s_getpc_b64", "s_add_u32", "s_addc_u32", "s_setpc_b64"]
        );
        assert_eq!(p.blocks[0].exit, Exit::Jump(1));
        assert_eq!(p.blocks[1].offset, 28);
        // s_addc_u32 reads the carry s_add_u32 left in SCC.
        let i = &p.blocks[0].insts;
        assert_eq!(i[2].read(Reg::Scc), i[1].write(Reg::Scc));
    }

    #[test]
    fn reports_what_it_cannot_express() {
        let code: Vec<u8> = [0xBE80_2E01_u32, S_ENDPGM] // s_movrels_b32 s0, s1
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        assert_eq!(
            build(&code, false, 256),
            Err(ShaderError::Unsupported {
                offset: 0,
                what: "s_movrels_b32"
            })
        );
    }

    #[test]
    fn relative_vgpr_access_spans_up_to_the_vgpr_count() {
        let code: Vec<u8> = [
            0x7E04_8501_u32, // v_movreld_b32 v2, v1: v[2 + m0] = v1
            0x7E00_8704,     // v_movrels_b32 v0, v4: v0 = v[4 + m0]
            S_ENDPGM,
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
        let p = build(&code, false, 6).unwrap();
        let [movreld, movrels, _] = &p.blocks[0].insts[..] else {
            panic!()
        };
        let vgprs = |regs: &[(Reg, Value)]| -> Vec<u8> {
            regs.iter()
                .filter_map(|r| match r.0 {
                    Reg::V(v) => Some(v),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(vgprs(&movreld.writes), [2, 3, 4, 5]);
        // v1 is the source; v2-v5 are read for the lanes and indices not written.
        assert_eq!(vgprs(&movreld.reads), [1, 2, 3, 4, 5]);
        assert!(movreld.read(Reg::S(M0)).is_some());
        assert_eq!(vgprs(&movrels.writes), [0]);
        assert_eq!(vgprs(&movrels.reads), [4, 5, 0]);
        assert_eq!(movrels.read(Reg::V(4)), movreld.write(Reg::V(4)));
        assert!(build(&code, false, 4).is_err(), "v4 is past 4 VGPRs");
    }

    #[test]
    fn relative_reads_keep_vgpr_writes_unless_m0_proves_them_disjoint() {
        for (m0, overlaps) in [
            (0xBEFC_0381, true),  // s_mov_b32 m0, 1: reads parked v5
            (0xBEFC_0380, false), // s_mov_b32 m0, 0: reads just v4
            (0xBEFC_0307, true),  // s_mov_b32 m0, s7: can read v5
        ] {
            let code: Vec<u8> = [
                0xBE86_0387, // s_mov_b32 s6, 7
                0xD761_0005,
                0x0001_0006, // v_writelane_b32 v5, s6, 0
                m0,
                0x7E00_8704, // v_movrels_b32 v0, v4
                0xD760_0002,
                0x0001_0105, // v_readlane_b32 s2, v5, 0
                S_ENDPGM,
            ]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
            let p = build(&code, false, 6).unwrap();
            let save = &p.blocks[0].insts[1];
            assert_eq!(save.write(Reg::V(5)).is_some(), overlaps);
            if overlaps {
                let relative = &p.blocks[0].insts[3];
                assert_eq!(relative.read(Reg::V(5)), save.write(Reg::V(5)));
            } else {
                assert_eq!(save.name(), "s_mov_b32");
            }
        }
    }

    const MIXED_PARKS: [u32; 17] = [
        0xBE86_0387, // s_mov_b32 s6, 7
        0xD761_0005,
        0x0001_0006, // v_writelane_b32 v5, s6, 0
        0xBEFC_0309, // s_mov_b32 m0, s9
        0x7E00_8704, // v_movrels_b32 v0, v4 (may read v5)
        0xBE8A_0388, // s_mov_b32 s10, 8
        0xBE8B_0389, // s_mov_b32 s11, 9
        0xD761_0005,
        0x0001_040A, // v_writelane_b32 v5, s10, 2
        0xD761_0005,
        0x0001_060B, // v_writelane_b32 v5, s11, 3
        0xD760_0002,
        0x0001_0505, // v_readlane_b32 s2, v5, 2
        0xBE8C_0380, // s_mov_b32 s12, 0 (separates the restore halves)
        0xD760_0003,
        0x0001_0705, // v_readlane_b32 s3, v5, 3
        S_ENDPGM,
    ];

    #[test]
    fn a_relative_reader_keeps_the_early_slot_but_not_the_later_pair() {
        let p = build_words(&MIXED_PARKS, true);
        let at = |word: usize| {
            p.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.machine.offset == word * 4)
                .unwrap()
        };
        let physical = at(1).write(Reg::V(5)).unwrap();
        assert_eq!(at(4).read(Reg::V(5)), Some(physical));
        let save = at(9);
        let restore = at(11);
        assert_eq!(save.name(), "s_mov_b64");
        assert_eq!(restore.name(), "s_mov_b64");
        for &(reg, value) in &save.writes {
            assert_eq!(restore.read(reg), Some(value));
        }
    }

    #[test]
    fn a_backedge_to_a_relative_reader_keeps_the_later_slots_physical() {
        let mut words = MIXED_PARKS;
        words[16] = 0xBF85_FFF2; // s_cbranch_scc1 to the M0 setup in word 3
        let p = build_words(&[&words[..], &[S_ENDPGM]].concat(), true);
        let physical = p.blocks.iter().flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_writelane_b32")
            .count();
        assert_eq!(physical, 3);
    }

    #[test]
    fn a_relative_reader_between_the_saves_keeps_the_whole_pair_physical() {
        let words = [
            &MIXED_PARKS[..3],
            &MIXED_PARKS[5..9], // includes the save of lane 2
            &MIXED_PARKS[3..5], // observe lane 2, before lane 3 is saved
            &MIXED_PARKS[9..],
        ].concat();
        let p = build_words(&words, true);
        let restores = p.blocks.iter().flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_readlane_b32")
            .count();
        assert_eq!(restores, 2);
    }

    #[test]
    fn a_branch_around_the_save_keeps_both_halves_of_the_pair_physical() {
        // Skip the save pair on one path. The restore can see the VGPR's
        // original values, which aren't held in the virtual scalar slots.
        let words = [
            &MIXED_PARKS[..7],
            &[0xBF85_0004], // s_cbranch_scc1 to the first restore
            &MIXED_PARKS[7..],
        ].concat();
        let p = build_words(&words, true);
        let restores = p.blocks.iter().flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_readlane_b32")
            .count();
        assert_eq!(restores, 2);
    }

    const fn s_branch(simm16: i16) -> u32 {
        0xBF82_0000 | simm16 as u16 as u32
    }

    const fn s_cbranch_scc1(simm16: i16) -> u32 {
        0xBF85_0000 | simm16 as u16 as u32
    }

    #[test]
    fn reverse_order_diamond_chain_eliminates_trivial_phis() {
        const LAYERS: usize = 4096;
        let mut reverse = vec![
            0xBE81_0381, // s_mov_b32 s1, 1
            0xBE82_0382, // s_mov_b32 s2, 2
            s_branch(((LAYERS - 1) * 4) as i16),
        ];
        for k in 0..LAYERS {
            reverse.extend([
                0xBF06_0201, // s_cmp_eq_u32 s1, s2
                s_cbranch_scc1(1),
                0xBF80_0000, // s_nop 0
                if k == 0 {
                    s_branch((LAYERS * 4 - 4) as i16)
                } else {
                    s_branch(-8)
                },
            ]);
        }
        reverse.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

        let mut forward = vec![
            0xBE81_0381, // s_mov_b32 s1, 1
            0xBE82_0382, // s_mov_b32 s2, 2
            s_branch(0),
        ];
        for _ in 0..LAYERS {
            forward.extend([
                0xBF06_0201, // s_cmp_eq_u32 s1, s2
                s_cbranch_scc1(1),
                0xBF80_0000, // s_nop 0
                s_branch(0),
            ]);
        }
        forward.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

        let rev = build_words(&reverse, false);
        let fwd = build_words(&forward, false);
        assert!(rev.inputs.is_empty(), "{:?}", rev.inputs);
        assert!(fwd.inputs.is_empty(), "{:?}", fwd.inputs);
        assert!(rev.blocks.iter().all(|b| b.phis.is_empty()));
        assert!(fwd.blocks.iter().all(|b| b.phis.is_empty()));
        assert_eq!(rev.blocks.len(), fwd.blocks.len());

        let rev_s1 = rev.blocks[0].insts[0].write(Reg::S(1)).unwrap();
        let rev_s2 = rev.blocks[0].insts[1].write(Reg::S(2)).unwrap();
        let rev_exit = rev.blocks.last().unwrap();
        assert_eq!(rev_exit.insts[0].name(), "s_add_u32");
        assert_eq!(rev_exit.insts[0].read(Reg::S(1)), Some(rev_s1));
        assert_eq!(rev_exit.insts[0].read(Reg::S(2)), Some(rev_s2));

        let fwd_s1 = fwd.blocks[0].insts[0].write(Reg::S(1)).unwrap();
        let fwd_s2 = fwd.blocks[0].insts[1].write(Reg::S(2)).unwrap();
        let fwd_exit = fwd.blocks.last().unwrap();
        assert_eq!(fwd_exit.insts[0].name(), "s_add_u32");
        assert_eq!(fwd_exit.insts[0].read(Reg::S(1)), Some(fwd_s1));
        assert_eq!(fwd_exit.insts[0].read(Reg::S(2)), Some(fwd_s2));
    }

    #[test]
    fn remove_trivial_phis_compresses_reverse_chain_in_bounded_steps() {
        const CHAIN: usize = 4096;
        let mut template = build_words(&[0xBE80_0380, S_ENDPGM], false).blocks[0].insts[0].clone();
        template.writes.clear();
        let mut blocks = Vec::with_capacity(CHAIN);
        for k in 0..CHAIN {
            let v1 = Value(2 + 2 * k as u32);
            let v2 = Value(3 + 2 * k as u32);
            let (n1, n2) = if k + 1 < CHAIN {
                (Value(2 + 2 * (k + 1) as u32), Value(3 + 2 * (k + 1) as u32))
            } else {
                (Value(0), Value(1))
            };
            let mut inst = template.clone();
            inst.reads = vec![(Reg::S(1), v1), (Reg::S(2), v2)];
            blocks.push(Block {
                offset: k * 4,
                preds: vec![],
                phis: vec![
                    Phi {
                        reg: Reg::S(1),
                        value: v1,
                        args: vec![n1, n1],
                    },
                    Phi {
                        reg: Reg::S(2),
                        value: v2,
                        args: vec![n2, n2],
                    },
                ],
                insts: vec![inst],
                exit: Exit::End,
            });
        }
        let steps = remove_trivial_phis_steps(&mut blocks, 2 + 2 * CHAIN as u32);
        assert!(blocks.iter().all(|b| b.phis.is_empty()));
        assert!(
            blocks
                .iter()
                .all(|b| b.insts[0].reads == [(Reg::S(1), Value(0)), (Reg::S(2), Value(1))])
        );
        assert!(steps < 100_000, "steps = {steps}");
    }

    #[test]
    fn non_trivial_phis_preserve_compressed_representatives() {
        const LAYERS: usize = 4;
        let mut code = vec![
            0xBE81_0381, // s_mov_b32 s1, 1
            0xBE82_0382, // s_mov_b32 s2, 2
            0xBF06_8000, // s_cmp_eq_u32 s0, 0
            s_cbranch_scc1((2 + (LAYERS - 1) * 4) as i16),
            0xBE81_0383, // s_mov_b32 s1, 3
            s_branch((LAYERS * 4) as i16),
        ];
        for k in 0..LAYERS {
            code.extend([
                0xBF06_0201, // s_cmp_eq_u32 s1, s2
                s_cbranch_scc1(1),
                0xBF80_0000, // s_nop 0
                if k == 0 {
                    s_branch((LAYERS * 4 - 4) as i16)
                } else {
                    s_branch(-8)
                },
            ]);
        }
        code.extend([0x8000_0201, S_ENDPGM]); // s_add_u32 s0, s1, s2

        let p = build_words(&code, false);
        let entry_s1 = p.blocks[0].insts[0].write(Reg::S(1)).unwrap();
        let entry_s2 = p.blocks[0].insts[1].write(Reg::S(2)).unwrap();
        let arm_s1 = p.blocks[1].insts[0].write(Reg::S(1)).unwrap();
        let merge = p.blocks.last().unwrap();
        for b in &p.blocks[..p.blocks.len() - 1] {
            assert!(b.phis.is_empty(), "{:?}", b.phis);
        }
        let [phi] = &merge.phis[..] else {
            panic!("{:?}", merge.phis)
        };
        assert_eq!(phi.reg, Reg::S(1));
        assert_eq!(phi.args, [arm_s1, entry_s1]);
        assert_eq!(merge.insts[0].read(Reg::S(1)), Some(phi.value));
        assert_eq!(merge.insts[0].read(Reg::S(2)), Some(entry_s2));

        let mut loop_code = vec![
            0xBE81_0381, // s_mov_b32 s1, 1
            0xBE82_0382, // s_mov_b32 s2, 2
            s_branch(((LAYERS - 1) * 4) as i16),
        ];
        for k in 0..LAYERS {
            loop_code.extend([
                0xBF06_0201, // s_cmp_eq_u32 s1, s2
                s_cbranch_scc1(1),
                0xBF80_0000, // s_nop 0
                if k == 0 {
                    s_branch((LAYERS * 4 - 4) as i16)
                } else {
                    s_branch(-8)
                },
            ]);
        }
        loop_code.extend([
            0x8001_0201, // s_add_u32 s1, s1, s2
            0xBF07_8401, // s_cmp_lg_u32 s1, 4
            s_cbranch_scc1(-3),
            0x8000_0201, // s_add_u32 s0, s1, s2
            S_ENDPGM,
        ]);

        let lp = build_words(&loop_code, false);
        let lp_s1 = lp.blocks[0].insts[0].write(Reg::S(1)).unwrap();
        let lp_s2 = lp.blocks[0].insts[1].write(Reg::S(2)).unwrap();
        let header = &lp.blocks[lp.blocks.len() - 2];
        let exit = lp.blocks.last().unwrap();
        for b in &lp.blocks[..lp.blocks.len() - 2] {
            assert!(b.phis.is_empty(), "{:?}", b.phis);
        }
        assert!(exit.phis.is_empty(), "{:?}", exit.phis);
        let [loop_phi] = &header.phis[..] else {
            panic!("{:?}", header.phis)
        };
        let next_s1 = header.insts[0].write(Reg::S(1)).unwrap();
        assert_eq!(loop_phi.reg, Reg::S(1));
        assert_eq!(loop_phi.args, [lp_s1, next_s1]);
        assert_eq!(header.insts[0].read(Reg::S(1)), Some(loop_phi.value));
        assert_eq!(header.insts[0].read(Reg::S(2)), Some(lp_s2));
        assert_eq!(exit.insts[0].read(Reg::S(1)), Some(next_s1));
        assert_eq!(exit.insts[0].read(Reg::S(2)), Some(lp_s2));
    }
}
