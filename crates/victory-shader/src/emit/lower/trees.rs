//! Descriptors every lane shares, chosen by uniform trees of joins and
//! selects: the heap index such a tree gives, made at its joins and on its
//! arms.

use super::{Lower, Val, Why};
use crate::emit::bitcode::V;
use crate::resource::Source;

impl Lower<'_> {
    /// The heap index of a descriptor every lane shares, computed in the
    /// current block. One chosen by control flow is a phi at its join of the
    /// index each predecessor computes at its end, one chosen by SCC a
    /// select of the index of each choice; either may hold the other
    /// (nested if/else, a select on one edge, a join on one arm). A join
    /// whose back edge is not lowered yet (a loop header) gets its phi
    /// filled in [`Self::fill_phis`].
    pub(super) fn uniform_heap_index(&mut self, src: &Source, sampler: bool) -> Result<V, Why> {
        match src {
            Source::Loaded { load, first } => self.heap_index(*load, *first, sampler),
            Source::Select { scc, args } => {
                let [a, b] = &args[..] else {
                    return Err("a descriptor chosen by a condition");
                };
                let cond = match self.get(*scc)? {
                    Val::Bit(x) => x,
                    _ => return Err("SCC without a value"),
                };
                let set = self.uniform_heap_index(a, sampler)?;
                let clear = self.uniform_heap_index(b, sampler)?;
                Ok(self.s.m.select(self.cur, cond, set, clear))
            }
            Source::Phi { block, args } => {
                let join = self.block[*block].ok_or("a descriptor chosen by control flow")?;
                let phi = self.s.m.phi(join, self.s.t.i32);
                let ready = self.x.p.blocks[*block]
                    .preds
                    .iter()
                    .all(|&q| self.block[q].is_none() || self.tail[q].is_some());
                if ready {
                    self.fill_descriptor_phi(phi, *block, args, sampler)?;
                } else {
                    self.pending_descriptors
                        .push((phi, *block, args.clone(), sampler));
                }
                Ok(phi)
            }
            _ => Err("a descriptor chosen by control flow"),
        }
    }

    /// Fills `phi`, the heap index at join `block`: what each predecessor's
    /// source gives at the end of its edge.
    pub(super) fn fill_descriptor_phi(
        &mut self,
        phi: V,
        block: usize,
        args: &[Source],
        sampler: bool,
    ) -> Result<(), Why> {
        let mut incoming = Vec::new();
        for (&q, arg) in self.x.p.blocks[block].preds.iter().zip(args) {
            if self.block[q].is_none() {
                continue;
            }
            let tail = self.tail[q].ok_or("a descriptor chosen by control flow")?;
            let at = self.edges.get(&(q, block)).copied().unwrap_or(tail);
            // No lane comes this way (`resource` gave it another edge's
            // source): any index will do.
            if self.never_taken(q, block) {
                incoming.push((self.s.m.i32(0), at));
                continue;
            }
            let here = std::mem::replace(&mut self.cur, at);
            let here_ir = std::mem::replace(&mut self.cur_ir, q);
            let index = self.uniform_heap_index(arg, sampler);
            self.cur = here;
            self.cur_ir = here_ir;
            incoming.push((index?, at));
        }
        self.s.m.set_phi(phi, incoming);
        Ok(())
    }

    /// Whether a load that a waterfall loop reads per lane is among `src`'s
    /// leaves: a handle made from it is not uniform.
    pub(super) fn waterfall_leaf(&self, src: &Source) -> bool {
        let waterfall = &self.x.res.waterfall.loads;
        src.leaves()
            .iter()
            .any(|l| matches!(l, Source::Loaded { load, .. } if waterfall.contains(load)))
    }
}
