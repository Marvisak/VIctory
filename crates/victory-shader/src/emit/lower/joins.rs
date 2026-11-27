//! Scalars at per-lane joins (issue #3562). On the console a scalar
//! register is one value per wave: where some lanes of a wave take a branch
//! and the rest skip it, the wave runs the branch and every lane reads what
//! it left after the join. The per-lane form keeps each lane's own path's
//! value, so the lanes that skipped the branch still read the old one.
//! [`Lower::wave_scalars`] gives every lane the wave's value instead, where
//! one host wave holds one console wave.

use super::*;

impl Lower<'_> {
    /// Each predecessor of IR block `b` (by its index in `preds`): its rank
    /// in code order, 0 the first.
    pub(super) fn pred_ranks(&self, b: usize) -> Vec<u32> {
        let p = self.x.p;
        let preds = &p.blocks[b].preds;
        preds
            .iter()
            .map(|&q| {
                let at = p.blocks[q].offset;
                preds.iter().filter(|&&r| p.blocks[r].offset < at).count() as u32
            })
            .collect()
    }

    /// At the start of IR block `b`, after its phis: a scalar the console
    /// wave holds is one value. Where its lanes reach `b` along different
    /// edges (a per-lane join), the per-lane form gives each lane the value
    /// along its own edge, so lanes that skipped a branch keep what the
    /// branch replaced; the console's wave ran every path some lane took,
    /// in code order, and holds what the last of them left (issue #3562).
    /// So each scalar phi takes, in every lane, the value of a lane that
    /// came from the latest predecessor any lane came from. Only with the
    /// console's lanes (one host wave is the console's), not at loop
    /// headers, and not past a loop between the branch and the join.
    pub(super) fn wave_scalars(&mut self, b: usize) {
        let p = self.x.p;
        if !self.console_lanes || self.s.mesh.is_some() || self.uniform_join(b) {
            return;
        }
        let preds: Vec<usize> = p.blocks[b]
            .preds
            .iter()
            .copied()
            .filter(|&q| self.block[q].is_some())
            .collect();
        if preds.len() < 2 || preds.iter().any(|&q| self.rpo[b] <= self.rpo[q]) {
            return;
        }
        // A loop between the branch and the join: lanes leave it on their
        // own rounds, and the per-lane form's scalars there are each lane's
        // own (a round count), not the wave's, so no one lane's is the
        // console's.
        let from = preds.iter().map(|&q| self.rpo[q]).min().unwrap_or(0);
        if self
            .counters
            .keys()
            .any(|&h| from < self.rpo[h] && self.rpo[h] < self.rpo[b])
        {
            return;
        }
        let scalars: Vec<(Value, V)> = p.blocks[b]
            .phis
            .iter()
            .filter(|phi| matches!(phi.reg, Reg::S(c) if c != EXEC && c != EXEC + 1))
            .filter(|phi| {
                !self.x.st.masks.contains_key(&phi.value)
                    && !self.x.st.highs.contains_key(&phi.value)
            })
            .filter_map(|phi| match self.vals.get(&phi.value) {
                Some(&Val::I32(x)) => Some((phi.value, x)),
                _ => None,
            })
            .collect();
        if scalars.is_empty() {
            return;
        }
        let c = self.cur;
        let i32t = self.s.t.i32;
        let rank = self.s.m.phi(c, i32t);
        self.edge_ranks.push((rank, b));
        let (max, min, unsigned) = (self.s.m.i8(3), self.s.m.i8(2), self.s.m.i8(1));
        let last = self.s.op(
            c,
            "waveActiveOp.i32",
            Attr::NoUnwind,
            i32t,
            119,
            &[rank, max, unsigned],
        );
        let mine = self.s.m.cmp(c, pred::EQ, rank, last);
        let lane = self.lane_index();
        let none = self.s.m.i32(u32::MAX);
        let candidate = self.s.m.select(c, mine, lane, none);
        let from = self.s.op(
            c,
            "waveActiveOp.i32",
            Attr::NoUnwind,
            i32t,
            119,
            &[candidate, min, unsigned],
        );
        self.s.flags |= flags::WAVE_OPS;
        for (value, x) in scalars {
            let wave = self.read_lane(x, from);
            self.vals.insert(value, Val::I32(wave));
        }
    }
}
