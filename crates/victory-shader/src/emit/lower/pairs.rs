//! Pair meetings ([`crate::emit::Options::pair_meetings`]): the console flow
//! for a thread group of several console waves on a host without 64-lane
//! waves.
//!
//! In the console flow each console wave takes its branches as the console
//! does, wave-wide; two console waves of a group may go different ways (a
//! loop as long as the wave's index, say), so a group barrier between the
//! two halves of one console wave would wait for threads that are elsewhere.
//! Instead the two host waves of a console wave meet on their own: an LDS
//! counter per console wave that each host wave counts up (one lane) and
//! reads until the other has counted too ([`Lower::meet`]). Both halves run
//! the same blocks in the same order (every branch decision is the console
//! wave's, exchanged), so they meet equally often and in step: when a half
//! counts in for its k-th meeting, the counter is 2k-2 or 2k-1, and it
//! reaches 2k only when both have. What either half wrote to LDS before a
//! meeting (with a group-shared fence before the count and after the wait)
//! the other reads after it: an exchange ([`Lower::pair_exchange`]) and
//! the order of the console wave's LDS accesses are exact as with a group
//! barrier. Console waves are ordered among themselves only by the title's
//! `s_barrier`s, as on the console, which stay group barriers: the console
//! waves must meet them in one order, each once, as far as each wave goes
//! ([`barriers_in_step`]).
//!
//! Idea from the half-wave pair meetings of trustno1-coder/VIctory-NVIDIA
//! (`emit/lower.rs`: `with_pair_meetings`, `Lower::meet`,
//! `exchange_dword`; foxhound lineage), re-implemented on the console flow.

use super::{LANE_MASK, Lower, Val, reads_mask_as_data};
use crate::emit::bitcode::{Attr, Bin, Rmw, Term, V, pred};
use crate::emit::shader::flags;
use crate::ir::{Inst, Program, Reg, Value};

/// Why a console wave64 program whose group holds several console waves
/// stays out of the console flow on a host without 64-lane waves.
pub(crate) const SEVERAL_CONSOLE_WAVES: &str =
    "console wave64 lanes in a group of more than 64 threads on a host without 64-lane waves";

/// Pair meetings on or off for this thread's lowerings until the guard
/// drops (set for each program by `emit_with`).
pub(in crate::emit) struct PairMeetings(bool);

thread_local! {
    /// Whether this thread's lowering makes pair meetings.
    static PAIR_MEETINGS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl PairMeetings {
    /// Pair meetings on or off for this thread until the guard drops.
    pub(in crate::emit) fn set(on: bool) -> PairMeetings {
        PairMeetings(PAIR_MEETINGS.replace(on))
    }

    /// Whether this thread's lowering makes pair meetings.
    pub(super) fn on() -> bool {
        PAIR_MEETINGS.get()
    }
}

impl Drop for PairMeetings {
    fn drop(&mut self) {
        PAIR_MEETINGS.set(self.0);
    }
}

/// Polls of a meeting's counter before a host wave goes on without the
/// other half: never reached while both run the program (the other half
/// is at most a memory access behind); a half cut out of a loop at
/// [`super::LOOP_LIMIT`] has left the program, and the wait must end. About
/// a millisecond of polling or more, short of the driver's reset.
const MEETING_POLLS: u32 = 1 << 20;

#[cfg(test)]
thread_local! {
    /// A test's poll limit for this thread's meetings (else
    /// [`MEETING_POLLS`]): a much lower one that still gives exact results
    /// shows how far the waits stay below it.
    pub(in crate::emit) static TEST_MEETING_POLLS: std::cell::Cell<Option<u32>> =
        const { std::cell::Cell::new(None) };
}

fn meeting_polls() -> u32 {
    #[cfg(test)]
    if let Some(n) = TEST_MEETING_POLLS.get() {
        return n;
    }
    MEETING_POLLS
}

/// Group-shared dwords the meetings of a group of `waves` console waves
/// take ([`Lower::pair_init`]): a counter per console wave and the ticket,
/// and two buffers of two exchange dwords per console wave. With the
/// program's own LDS they must fit the host's 32 KiB.
pub(super) fn state_dwords(waves: u32) -> u32 {
    waves + 1 + 4 * waves
}

/// Why a console wave64 program whose group holds several console waves
/// stays out of the console flow in halves: its barriers
/// ([`barriers_in_step`]).
pub(crate) const BARRIERS_OUT_OF_STEP: &str =
    "title barriers not met in one order by the console waves of a group in halves";

/// Whether the console waves of a group meet the title's barriers in step:
/// no block holding an `s_barrier` is in a cycle, and of any two such
/// blocks one dominates the other. The barriers are then a chain, and a
/// console wave meets a first stretch of it, each barrier once: up to where
/// the wave ends, or goes a way that holds no barrier. A console wave may so
/// end before a barrier the others meet (all its lanes switched off: the
/// title's classification programs, whose threads past the count leave at
/// the top); the barrier goes on without it, on the console and on the host
/// alike, both waiting only for threads still running. What stays out is a
/// barrier in a loop (nothing shows that every console wave runs it equally
/// often) and two barriers on different ways (console waves could wait at
/// different ones).
pub(super) fn barriers_in_step(p: &Program) -> bool {
    let n = p.blocks.len();
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut index = vec![usize::MAX; n];
    for (i, &b) in order.iter().enumerate() {
        index[b] = i;
    }
    let idom = crate::ir::idoms(&p.blocks, &order, &index);
    let dominates = |a: usize, mut b: usize| {
        while b != a && idom[b] != b {
            b = idom[b];
        }
        b == a
    };
    // Whether a way leads from `from` back to it.
    let in_cycle = |from: usize| {
        let mut seen = vec![false; n];
        let mut work = p.blocks[from].exit.successors();
        while let Some(b) = work.pop() {
            if b == from {
                return true;
            }
            if !std::mem::replace(&mut seen[b], true) {
                work.extend(p.blocks[b].exit.successors());
            }
        }
        false
    };
    // (blocks nothing leads to are not run)
    let barriers: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&b| p.blocks[b].insts.iter().any(|i| i.name() == "s_barrier"))
        .collect();
    barriers.iter().all(|&b| !in_cycle(b))
        && barriers
            .iter()
            .enumerate()
            .all(|(i, &a)| barriers[i + 1..].iter().all(|&b| dominates(a, b)))
}

impl Lower<'_> {
    /// Pair meetings: the group-shared counters and exchange dwords, this
    /// thread's console thread and console wave. In the entry block (every
    /// thread runs it, before any branch): thread n zeroes counter n and
    /// the last thread past them a ticket; the group meets once; then each
    /// host wave takes the next 32 console threads from the ticket. A host
    /// need not give a host wave an aligned run of flat thread ids (2-D
    /// groups), so the relabelling of [`Lower::console_flat`] is a
    /// bijection only this way (as trustno1-coder/VIctory-NVIDIA's
    /// `wave_base`, after Vice-Tay c125c0c). Every host wave is whole (the
    /// group a multiple of 64 threads) and all its lanes run here.
    pub(super) fn pair_init(&mut self) {
        let waves = self.pairs.expect("pair meetings");
        let (e, i1t, i32t, void) = (self.s.entry, self.s.t.i1, self.s.t.i32, self.s.t.void);
        let counters = self.s.m.array(waves + 1, i32t);
        let counters = self.s.m.global("meetings", counters, 3);
        let words = self.s.m.array(4 * waves, i32t);
        let words = self.s.m.global("pair_xchg", words, 3);
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let last = self.s.m.i32(waves);
        let past = self.s.m.cmp(e, pred::IUGT, flat, last);
        let n = self.s.m.select(e, past, last, flat);
        let mine = self.s.m.element(e, counters, n);
        let z = self.s.m.i32(0);
        self.s.m.store(e, mine, z);
        let mode = self.s.m.i32(1 | 8);
        self.s
            .op(e, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
        // (every lane adds, the first 32: no branch in the entry block)
        let first = self
            .s
            .op(e, "waveIsFirstLane", Attr::NoUnwind, i1t, 110, &[]);
        let k32 = self.s.m.i32(32);
        let add = self.s.m.select(e, first, k32, z);
        let ticket = self.s.m.element(e, counters, last);
        let old = self.s.m.atomic(e, Rmw::Add, ticket, add);
        let base = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[old],
        );
        let lane = self
            .s
            .op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let f = self.s.m.bin(e, Bin::Add, base, lane);
        self.s.flags |= flags::WAVE_OPS;
        self.console_flat = Some(f);
        let six = self.s.m.i32(6);
        let wave = self.s.m.bin(e, Bin::LShr, f, six);
        let counter = self.s.m.element(e, counters, wave);
        self.pair_slots = Some((counter, words, wave));
    }

    /// Pair meetings: the two host waves of this thread's console wave
    /// meet, at [`Lower::cur`]; the code after it goes on in a new block
    /// (`cur` again). What either wrote to group-shared memory before, the
    /// other reads after. One lane of each host wave counts it in; each
    /// wave reads the counter (one dword all lanes share) until it reaches
    /// the next even number, at most [`MEETING_POLLS`] times.
    pub(super) fn meet(&mut self) {
        let (counter, ..) = self.pair_slots.expect("pair meetings");
        let (i1t, i32t, void) = (self.s.t.i1, self.s.t.i32, self.s.t.void);
        let (fence, zero, one) = (self.s.m.i32(8), self.s.m.i32(0), self.s.m.i32(1));
        let c = self.cur;
        self.s
            .op(c, "barrier", Attr::NoDuplicate, void, 80, &[fence]);
        let first = self
            .s
            .op(c, "waveIsFirstLane", Attr::NoUnwind, i1t, 110, &[]);
        self.s.flags |= flags::WAVE_OPS;
        let at = self.begin_guarded(Some(first));
        let old = self.s.m.atomic(self.cur, Rmw::Add, counter, one);
        let old = self.end_guarded(at, &[(old, zero)])[0];
        let c = self.cur;
        let old = self.s.op(
            c,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[old],
        );
        let (even, two) = (self.s.m.i32(!1), self.s.m.i32(2));
        let base = self.s.m.bin(c, Bin::And, old, even);
        let target = self.s.m.bin(c, Bin::Add, base, two);
        let (wait, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(c, Term::Br(wait));
        let n = self.s.m.phi(wait, i32t);
        self.s
            .op(wait, "barrier", Attr::NoDuplicate, void, 80, &[fence]);
        let now = self.s.m.load(wait, counter);
        let now = self.s.op(
            wait,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[now],
        );
        let short = self.s.m.bin(wait, Bin::Sub, now, target);
        let early = self.s.m.cmp(wait, pred::SLT, short, zero);
        let next = self.s.m.bin(wait, Bin::Add, n, one);
        let limit = self.s.m.i32(meeting_polls());
        let more = self.s.m.cmp(wait, pred::IULT, next, limit);
        let again = self.s.m.bin(wait, Bin::And, early, more);
        self.s.m.terminate(wait, Term::CondBr(again, wait, done));
        self.s.m.set_phi(n, vec![(zero, c), (next, wait)]);
        self.s
            .op(done, "barrier", Attr::NoDuplicate, void, 80, &[fence]);
        self.cur = done;
    }

    /// [`Lower::exchange`] with pair meetings, at [`Lower::cur`] only (a
    /// meeting splits the block being lowered). Each console wave has two
    /// buffers of two dwords (lower half's, upper half's); a half that has
    /// met m times finds the counter at 2m or 2m + 1 and takes buffer m % 2.
    /// The other half takes the same one (it has met m times as well), and
    /// neither writes it again before the meeting after next, which needs
    /// the other past its reads of this one.
    pub(super) fn pair_exchange(&mut self, at: usize, x: V) -> (V, V) {
        let z = self.s.m.i32(0);
        if at != self.cur {
            self.pair_fail
                .get_or_insert("an exchange of the halves outside the block being lowered");
            return (z, z);
        }
        let (counter, words, wave) = self.pair_slots.expect("pair meetings");
        let i32t = self.s.t.i32;
        let (_, half) = self.console_lane();
        let c = self.cur;
        let now = self.s.m.load(c, counter);
        let now = self.s.op(
            c,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[now],
        );
        let (one, two) = (self.s.m.i32(1), self.s.m.i32(2));
        let met = self.s.m.bin(c, Bin::LShr, now, one);
        let buffer = self.s.m.bin(c, Bin::And, met, one);
        let buffer = self.s.m.bin(c, Bin::Shl, buffer, one);
        let first = self.s.m.bin(c, Bin::Shl, wave, two);
        let first = self.s.m.bin(c, Bin::Add, first, buffer);
        let own = self.s.m.bin(c, Bin::Add, first, half);
        let own = self.s.m.element(c, words, own);
        self.s.m.store(c, own, x);
        self.meet();
        let c = self.cur;
        let second = self.s.m.bin(c, Bin::Add, first, one);
        let p0 = self.s.m.element(c, words, first);
        let p1 = self.s.m.element(c, words, second);
        (self.s.m.load(c, p0), self.s.m.load(c, p1))
    }

    /// Pair meetings: the ballots of the lane masks `inst` reads as
    /// numbers, and the console-wave SCC it reads, exchanged before its own
    /// code is lowered. A meeting starts a new block, and an instruction's
    /// code goes on in the block it began in; made here, its code finds
    /// them made. What fails here fails again where the instruction reads
    /// it, and is told there. (As trustno1-coder/VIctory-NVIDIA's
    /// `exchange_first`.)
    pub(super) fn exchange_first(&mut self, inst: &Inst) {
        let st = self.x.st;
        let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
        let name = inst.name();
        if reads_mask_as_data(st, inst)
            || name == "v_readfirstlane_b32"
            || name.starts_with("v_mbcnt")
        {
            for &(r, v) in &inst.reads {
                if matches!(r, Reg::S(_)) && is_mask(v) {
                    let _ = self.mask_dword(v);
                }
            }
        }
        if let Some(v) = inst.read(Reg::Scc)
            && matches!(self.vals.get(&v), None | Some(Val::Missing(LANE_MASK)))
        {
            let _ = self.scc_bit(v);
        }
    }
}
