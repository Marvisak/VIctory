//! Group barriers and how the lanes of a wave reach them: the barriers a
//! block holds, loops holding them, branches decided for the whole wave as
//! the console decides them, barriers added where lanes skip some, and
//! console mode for programs whose barriers cannot be counted. Split from
//! `lower.rs`, which stays under the asset policy's size limit.

use super::*;

/// Console mode's latches going round through a block that only jumps back
/// to the header ([`Lower::park_lanes`]): by such block, its header; and by
/// header, what the branch before one parks, for its back edge.
#[derive(Default)]
pub(super) struct Rounds {
    via: HashMap<usize, usize>,
    next: HashMap<usize, V>,
}

impl<'a> Lower<'a> {
    /// Console mode's parked lanes ([`Lower::parked`]): a phi of whether a
    /// lane is parked at every loop header, set on entry for a lane the
    /// wave carries in ([`Lower::park_entry`]). A latch whose exit is
    /// decided for the wave parks the lanes that would have left
    /// ([`Lower::wave_branch`]), and every instruction in the loop is then
    /// guarded by `!parked` too ([`Lower::unparked`]). The console
    /// compiler's waterfall goes round through a block that only restores
    /// EXEC (`s_and_b64 exec, s[18:19], s[16:17]`) and jumps back: the
    /// branch before such a block parks as a latch does ([`Rounds`]).
    // shortcut: a lane is parked only at such a latch whose other side
    // leaves the loop (the do-while shape); a branch going round the loop
    // from the middle keeps its lanes running to the latch.
    pub(super) fn park_lanes(&mut self, order: &[usize]) {
        let loops = self.barrier_loops(order);
        let headers: Vec<usize> = self.counters.keys().copied().collect();
        for h in headers {
            // The natural loop: the header and every block reaching one of
            // its latches without passing it. Only those may read the
            // header's phi (an RPO range also holds blocks beside the loop,
            // which the header does not dominate).
            let mut body = vec![false; self.x.p.blocks.len()];
            body[h] = true;
            let mut todo: Vec<usize> = loops.iter().filter(|l| l.0 == h).map(|l| l.1).collect();
            if todo.is_empty() {
                continue;
            }
            let latches = todo.clone();
            while let Some(x) = todo.pop() {
                if body[x] || self.block[x].is_none() {
                    continue;
                }
                body[x] = true;
                todo.extend(self.x.p.blocks[x].preds.iter().copied());
            }
            // Only a loop one of whose latches parks lanes: its exit decided
            // for the wave, the other side leaving the loop. Every other
            // loop is lowered as before (no phi, no guard more).
            let parks = latches.iter().any(|&l| {
                self.wave_branches.contains_key(&l)
                    && matches!(self.x.st.exits[l], LaneExit::Branch { taken, not_taken, .. }
                        if (taken == h && !body[not_taken]) || (not_taken == h && !body[taken]))
            });
            // A latch only jumping back, after a branch decided for the wave
            // whose other side leaves the loop (`Rounds`).
            let via: Vec<usize> = latches
                .iter()
                .copied()
                .filter(|&l| {
                    let [x] = self.x.p.blocks[l].preds[..] else {
                        return false;
                    };
                    let round = |to: usize, out: usize| to == l && !body[out];
                    matches!(self.x.st.exits[l], LaneExit::Jump(t) if t == h)
                        && self.wave_branches.contains_key(&x)
                        && matches!(self.x.st.exits[x], LaneExit::Branch { taken, not_taken, .. }
                            if round(taken, not_taken) || round(not_taken, taken))
                })
                .collect();
            if !parks && via.is_empty() {
                continue;
            }
            for l in via {
                self.rounds.via.insert(l, h);
            }
            let blk = self.block[h].expect("reachable");
            let phi = self.s.m.phi(blk, self.s.t.i1);
            self.parked.insert(h, (phi, Vec::new(), body));
        }
    }

    /// A forward edge from IR block `from` (DXIL block `via`) into header
    /// `to`: a lane enters parked unless it comes on its own path (it
    /// reaches `from`, and `takes`, its own side of a branch the wave
    /// decided). A lane the wave carries in runs nothing in the loop.
    pub(super) fn park_entry(&mut self, from: usize, to: usize, via: usize, takes: Option<V>) {
        if !self.parked.contains_key(&to) {
            return;
        }
        let c = self.cur;
        let start = match self.boolean(self.x.st.reach[from], c) {
            Ok(on) => {
                let on = match takes {
                    Some(t) => self.s.m.bin(c, Bin::And, on, t),
                    None => on,
                };
                let yes = self.s.m.bool(true);
                self.s.m.bin(c, Bin::Xor, on, yes)
            }
            Err(_) => self.s.m.bool(false),
        };
        self.parked.get_mut(&to).expect("parked").1.push((start, via));
    }

    /// A back edge to header `to` from DXIL block `c`: parked lanes stay
    /// parked ([`Lower::wave_branch`] may park more, and a latch going
    /// round through a jump parks them here).
    pub(super) fn park_round(&mut self, to: usize, c: usize) {
        let next = self.rounds.next.remove(&to);
        if let Some((parked, incoming, _)) = self.parked.get_mut(&to) {
            incoming.push((next.unwrap_or(*parked), c));
        }
    }

    /// `guard` with the lanes parked in every loop holding block `b` left
    /// out.
    pub(super) fn unparked(&mut self, b: usize, guard: Option<V>) -> Option<V> {
        let parked: Vec<V> = self
            .parked
            .iter()
            .filter(|(_, p)| p.2[b])
            .map(|(_, p)| p.0)
            .collect();
        let mut guard = guard;
        for phi in parked {
            let c = self.cur;
            let yes = self.s.m.bool(true);
            let running = self.s.m.bin(c, Bin::Xor, phi, yes);
            guard = Some(match guard {
                Some(g) => self.s.m.bin(c, Bin::And, g, running),
                None => running,
            });
        }
        guard
    }

    /// The terminator of block `b`, a per-lane branch decided for the
    /// whole wave ([`Lower::wave_loop_exits`], console mode).
    pub(super) fn wave_branch(
        &mut self,
        b: usize,
        when: Bool,
        taken: usize,
        not_taken: usize,
        offset: usize,
    ) -> Result<Term, Stop> {
        // The console's wave-wide test (`Lower::wave_loop_exits`):
        // the per-lane condition in all (or any) active lanes.
        let all = self.wave_branches[&b];
        // In console mode lanes reach blocks off their own paths:
        // the condition as the lane has it there, not simplified
        // by the path it would have taken.
        let when = if self.console_mode {
            self.x.st.whens[b].unwrap_or(when)
        } else {
            when
        };
        let lane = self
            .boolean(when, self.cur)
            .map_err(|w| unsupported(offset, w))?;
        let i1 = self.s.t.i1;
        let (name, opcode) = if all { ("waveAllTrue", 114) } else { ("waveAnyTrue", 113) };
        // Parked lanes (`Lower::parked`) are out of the wave's test, as the
        // console's EXEC leaves them out: their frozen registers may still
        // say "go on", which would keep the wave in the loop for ever. Any
        // lane: only unparked ones count; all lanes: parked ones agree.
        let voting = if all {
            let c = self.cur;
            let yes = self.s.m.bool(true);
            let against = self.s.m.bin(c, Bin::Xor, lane, yes);
            let against = self.unparked(b, Some(against)).expect("a guard");
            let c = self.cur;
            self.s.m.bin(c, Bin::Xor, against, yes)
        } else {
            self.unparked(b, Some(lane)).expect("a guard")
        };
        let cond = self
            .s
            .op(self.cur, name, Attr::NoUnwind, i1, opcode, &[voting]);
        self.s.flags |= flags::WAVE_OPS;
        let back = |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
        let cond = if back(self, taken) {
            let ok = self.count_iteration(taken, pred::IULT);
            self.s.m.bin(self.cur, Bin::And, cond, ok)
        } else if back(self, not_taken) {
            let stop = self.count_iteration(not_taken, pred::IUGE);
            self.s.m.bin(self.cur, Bin::Or, cond, stop)
        } else {
            cond
        };
        // A latch whose other side leaves the loop: lanes that would have
        // left while the wave goes round are parked (`Lower::parked`). It
        // may go round through a block that only jumps back (`Rounds`).
        let latch = [(taken, not_taken, false), (not_taken, taken, true)]
            .into_iter()
            .find_map(|(round, out, t)| {
                let h = if back(self, round) { round } else { *self.rounds.via.get(&round)? };
                self.parked.get(&h).is_some_and(|p| !p.2[out]).then_some((h, round, t))
            });
        if let Some((header, round, leaves_when_taken)) = latch {
            let c = self.cur;
            let leaves = if leaves_when_taken {
                lane
            } else {
                let yes = self.s.m.bool(true);
                self.s.m.bin(c, Bin::Xor, lane, yes)
            };
            let phi = self.parked[&header].0;
            let now = self.s.m.bin(c, Bin::Or, phi, leaves);
            if round != header {
                self.rounds.next.insert(header, now);
            } else if let Some(last) = self.parked.get_mut(&header).and_then(|p| p.1.last_mut()) {
                *last = (now, c);
            }
        }
        let (t, f) = (self.edge(b, taken), self.edge(b, not_taken));
        let yes = self.s.m.bool(true);
        let not_lane = self.s.m.bin(self.cur, Bin::Xor, lane, yes);
        for (to, via, takes) in [(taken, t, lane), (not_taken, f, not_lane)] {
            if !back(self, to) {
                let from = if Some(via) == self.block[to] { self.cur } else { via };
                self.enter_loop(b, to, from, Some(takes));
            }
        }
        Ok(Term::CondBr(cond, t, f))
    }

    /// Whether block `b`'s branch on a lane condition is taken when all
    /// active lanes have it (else when any has), as the console decides it;
    /// `None` for a block not ending in such a branch.
    ///
    /// The lane condition is a mask's bit per lane: a branch on VCC or EXEC
    /// being zero is taken when no lane has it, and SCC of a mask
    /// (`s_and_b64` and the like, `s_cmp_lg` of two masks) is set when any
    /// lane has it. `s_cmp_eq` of two masks sets SCC when all lanes are
    /// equal, so there the test is the other way round.
    // shortcut: only SCC set in `b` itself is looked at; one set in an
    // earlier block keeps the usual sense.
    pub(super) fn wave_test(&self, b: usize) -> Option<bool> {
        let insts = &self.x.p.blocks[b].insts;
        let (branch, before) = insts.split_last()?;
        let mut all = match branch.name() {
            "s_cbranch_vccz" | "s_cbranch_execz" | "s_cbranch_scc0" => true,
            "s_cbranch_vccnz" | "s_cbranch_execnz" | "s_cbranch_scc1" => false,
            _ => return None,
        };
        if branch.name().starts_with("s_cbranch_scc")
            && before
                .iter()
                .rev()
                .find(|i| i.write(Reg::Scc).is_some())
                .is_some_and(|i| i.name().starts_with("s_cmp_eq_"))
        {
            all = !all;
        }
        Some(all)
    }

    /// A group barrier (`s_barrier`): thread-group sync with a shared-memory
    /// fence, and a UAV fence when the program writes UAVs (the waits before
    /// an s_barrier make those writes visible on the console).
    pub(super) fn group_barrier(&mut self, block: usize) {
        let mode = if self.writes_uavs { 1 | 2 | 8 } else { 1 | 8 };
        let mode = self.s.m.i32(mode);
        let void = self.s.t.void;
        self.s.op(block, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// Group barriers per lane-dependent branch edge, so that every lane
    /// of a group arrives at a barrier the same number of times. The
    /// console runs `s_barrier` per wave: a branch skips a block only when
    /// no lane of the wave needs it, and the wave runs the block, barrier
    /// included, with the other lanes masked off. On the host each lane
    /// takes its own branch, so lanes skipping a block with a barrier never
    /// arrive at it and the lanes inside wait for them forever (a GPU hang).
    /// A host barrier counts arrivals, not
    /// instructions, so the lighter side of such a branch gets as many
    /// barriers as the sides' counts to the program's end differ.
    ///
    /// Counts are taken over forward edges, so inside a loop with uniform
    /// exits they balance each iteration (every lane runs as many).
    /// Refused: a loop with a barrier and a lane-dependent exit, counts
    /// that vary along uniform paths inside a lane-dependent branch, and
    /// indirect jumps, where no count holds. Programs without barriers
    /// emission keeps get nothing.
    // shortcut: the extra barriers are at the start of the lighter edge, so
    // a lane there may read LDS after the join before the heavier side's
    // last writes before the join show (on the console those lanes are in
    // lockstep with the others).
    /// The group barriers emission keeps in IR block `b`.
    pub(super) fn block_barriers(&self, b: usize) -> u32 {
        if self.no_barrier.contains(&b) {
            return 0;
        }
        self.x.p.blocks[b]
            .insts
            .iter()
            .filter(|i| i.name() == "s_barrier")
            .count() as u32
    }

    /// Loops: back edge l -> h, the blocks between them in reverse
    /// postorder (an over-approximation), and whether one holds a barrier.
    pub(super) fn barrier_loops(&self, order: &[usize]) -> Vec<(usize, usize, bool)> {
        let st = self.x.st;
        let back = |from: usize, to: usize| self.rpo[to] <= self.rpo[from];
        order
            .iter()
            .flat_map(|&l| {
                let succ: Vec<usize> = match st.exits[l] {
                    LaneExit::Jump(t) => vec![t],
                    LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
                    _ => vec![],
                };
                succ.into_iter()
                    .filter(move |&h| self.block[h].is_some() && back(l, h))
                    .map(move |h| (h, l))
                    .collect::<Vec<_>>()
            })
            .map(|(h, l)| {
                let barrier = order.iter().any(|&x| {
                    self.rpo[h] <= self.rpo[x] && self.rpo[x] <= self.rpo[l] && self.block_barriers(x) > 0
                });
                (h, l, barrier)
            })
            .collect()
    }

    /// Per-lane branches leaving (or going round) a loop with a group
    /// barrier that are decided for the whole wave instead, as the console
    /// decides `s_cbranch_vccz`/`execz` (taken if the mask is empty: the
    /// lanes' condition in all active lanes) and `vccnz`/`execnz` (in any):
    /// by IR block, true for all. Per lane, lanes leaving such a loop at
    /// different iterations leave the others waiting at its barrier (a loop
    /// going round until each lane reaches a count of its own). Only where
    /// the wave's decision makes lanes
    /// leave the loop sooner, never stay longer: what follows the loop is
    /// reached by every lane per lane too, so its guards hold, and no lane
    /// runs an iteration its guards were not made for. Other loop exits
    /// keep the barrier counting ([`Lower::barrier_balance`]).
    pub(super) fn wave_loop_exits(&self, order: &[usize]) -> HashMap<usize, bool> {
        let st = self.x.st;
        let mut out = HashMap::new();
        let loops = self.barrier_loops(order);
        if !loops.iter().any(|l| l.2) {
            return out;
        }
        let back = |from: usize, to: usize| self.rpo[to] <= self.rpo[from];
        let inside = |(h, l): (usize, usize), x: usize| self.rpo[h] <= self.rpo[x] && self.rpo[x] <= self.rpo[l];
        for &b in order {
            let LaneExit::Branch { when, taken, not_taken } = st.exits[b] else {
                continue;
            };
            let lane = when != Bool::TRUE
                && when != Bool::FALSE
                && !st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_)));
            if !lane {
                continue;
            }
            let Some(all) = self.wave_test(b) else {
                continue;
            };
            let mut safe = None;
            for &(h, l, barrier) in &loops {
                let r = (h, l);
                if !inside(r, b) || !barrier {
                    continue;
                }
                // The side going on in the loop (round it included).
                let (ti, ni) = (inside(r, taken), inside(r, not_taken));
                let stays_taken = match (ti, ni) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => continue,
                };
                // All lanes must go on (taken, all) or none may leave
                // (not taken, any): the wave goes on only where each lane
                // would.
                let ok = stays_taken == all;
                if !ok || back(b, if stays_taken { not_taken } else { taken }) {
                    safe = Some(false);
                    break;
                }
                safe = Some(true);
            }
            if safe == Some(true) {
                out.insert(b, all);
            }
        }
        out
    }

    /// For a program whose barriers cannot be balanced: every per-lane
    /// branch on a lane mask, decided for the whole wave as the console
    /// decides it (by IR block, true where all active lanes must take it).
    /// With every vector instruction then guarded by its EXEC
    /// ([`Lower::guard`]), the wave walks every path any of its lanes needs
    /// with the others masked off, and its lanes arrive at each barrier
    /// together. Costs guards the per-lane form leaves out.
    // shortcut: a console wave is 64 lanes; a wave64 program run in host
    // waves of 32 decides per half, and halves whose lanes disagree still
    // part at a barrier.
    pub(super) fn console_branches(&self, order: &[usize]) -> HashMap<usize, bool> {
        let st = self.x.st;
        let mut out = HashMap::new();
        for &b in order {
            let LaneExit::Branch { when, .. } = st.exits[b] else {
                continue;
            };
            let when = st.whens[b].unwrap_or(when);
            let lane = when != Bool::TRUE
                && when != Bool::FALSE
                && !st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_)));
            if !lane {
                continue;
            }
            let Some(all) = self.wave_test(b) else {
                continue;
            };
            out.insert(b, all);
        }
        out
    }

    pub(super) fn barrier_balance(
        &self,
        order: &[usize],
    ) -> Result<HashMap<(usize, usize), u32>, (usize, Why)> {
        let p = self.x.p;
        let st = self.x.st;
        let barriers = |b: usize| self.block_barriers(b);
        let total: u32 = order.iter().map(|&b| barriers(b)).sum();
        let mut extra = HashMap::new();
        if total == 0 {
            return Ok(extra);
        }
        let offset = |b: usize| {
            p.blocks[b]
                .insts
                .last()
                .map_or(p.blocks[b].offset, |i| i.machine.offset)
        };
        // A branch decided for the whole wave (`Lower::wave_loop_exits`) is
        // a uniform one here.
        let lane = |b: usize, when: Bool| {
            when != Bool::TRUE
                && when != Bool::FALSE
                && !self.wave_branches.contains_key(&b)
                && !st
                    .bools
                    .support(when)
                    .iter()
                    .all(|a| matches!(a, Atom::Uniform(_)))
        };
        let back = |from: usize, to: usize| self.rpo[to] <= self.rpo[from];
        // Loops holding a barrier may only be left uniformly.
        let loops = self.barrier_loops(order);
        let inside = |(h, l): (usize, usize), x: usize| self.rpo[h] <= self.rpo[x] && self.rpo[x] <= self.rpo[l];
        // Whether a per-lane branch leaves a loop (or goes round it): a lane
        // going on there leaves it later the same way, so its side needs no
        // count of its own. Err if the loop holds a barrier.
        let leaves_loop = |b: usize, taken: usize, not_taken: usize| -> Result<Option<usize>, (usize, Why)> {
            let mut out = None;
            // Every loop the branch leaves or goes round, not just the first.
            for &(h, l, barrier) in &loops {
                let r = (h, l);
                if !inside(r, b) {
                    continue;
                }
                let (tb, nb) = (back(b, taken), back(b, not_taken));
                let (ti, ni) = (inside(r, taken) && !tb, inside(r, not_taken) && !nb);
                if ti && ni {
                    continue;
                }
                // Both sides leave this loop: a branch after it, not about it.
                if !tb && !nb && !inside(r, taken) && !inside(r, not_taken) {
                    continue;
                }
                if barrier {
                    return Err((offset(b), "a loop with a group barrier left per lane"));
                }
                // The forward side, whose count is the loop's way out: the
                // other side of a back edge (a latch, or a branch going
                // round), else the side leaving the loop. A back edge's own
                // count is 0 (counts are over forward edges).
                out = Some(if tb || (!nb && inside(r, taken)) { not_taken } else { taken });
            }
            Ok(out)
        };
        // Barriers from each block's start to the program's end over
        // forward edges: the counts possible along uniform paths (every
        // lane of a group takes the same one), sorted.
        // shortcut: two sides of a per-lane branch whose sets differ by a
        // constant are taken to differ by it along every uniform path; a
        // side with a uniform choice of its own before the join that the
        // other lacks could pair counts otherwise.
        const MOST: usize = 16;
        let shift = |set: &[u32], by: u32| -> Vec<u32> { set.iter().map(|c| c + by).collect() };
        let mut to_end: HashMap<usize, Vec<u32>> = HashMap::new();
        for &b in order.iter().rev() {
            let here = barriers(b);
            let after = |to: usize| -> Vec<u32> {
                if self.block[to].is_none() || back(b, to) {
                    vec![0]
                } else {
                    to_end.get(&to).cloned().unwrap_or_else(|| vec![0])
                }
            };
            let count = match st.exits[b] {
                LaneExit::End => vec![here],
                LaneExit::Indirect => {
                    return Err((offset(b), "an indirect jump in a program with group barriers"));
                }
                LaneExit::Jump(t) => shift(&after(t), here),
                LaneExit::Branch { when, taken, not_taken } => {
                    let (x, y) = (after(taken), after(not_taken));
                    let out = if lane(b, when) {
                        leaves_loop(b, taken, not_taken)?
                    } else {
                        None
                    };
                    if let Some(out) = out {
                        shift(&after(out), here)
                    } else if lane(b, when) {
                        // The same counts up to a constant: the lighter side
                        // gets that many more.
                        let (lo, hi, light) = if x[0] <= y[0] { (&x, &y, taken) } else { (&y, &x, not_taken) };
                        let n = hi[0] - lo[0];
                        // A side meeting no more barriers on any path goes
                        // on to its end and exits; a host barrier does not
                        // wait for threads that exited (an early return
                        // per lane before a barrier works on the host too).
                        // shortcut: D3D12 leaves barriers after a thread's
                        // exit undefined; NVIDIA and AMD count exited
                        // threads out, and `barrier_tests` checks it on the
                        // hardware adapter and WARP.
                        if lo[..] == [0] && (lo.len() != hi.len() || shift(lo, n) != *hi) {
                            let mut both = shift(hi, here);
                            both.dedup();
                            to_end.insert(b, both);
                            continue;
                        }
                        if lo.len() != hi.len() || shift(lo, n) != *hi {
                            return Err((
                                offset(b),
                                "group barriers counted differently along uniform paths inside a per-lane branch",
                            ));
                        }
                        if n > 0 {
                            if back(b, light) {
                                return Err((offset(b), "a loop with a group barrier left per lane"));
                            }
                            extra.insert((b, light), n);
                        }
                        shift(hi, here)
                    } else {
                        let mut both: Vec<u32> = x.iter().chain(&y).map(|c| c + here).collect();
                        both.sort_unstable();
                        both.dedup();
                        if both.len() > MOST {
                            return Err((offset(b), "too many uniform paths with group barriers"));
                        }
                        both
                    }
                }
            };
            to_end.insert(b, count);
        }
        Ok(extra)
    }
}
