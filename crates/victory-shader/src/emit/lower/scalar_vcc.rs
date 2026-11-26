use super::{Atom, Bin, Bool, Lower, Reg, StageIo, V, Val, Value, Why, pred};
use std::collections::{HashMap, HashSet};

impl Lower<'_> {
    /// An SGPR copied into VCC may already be a per-lane DXIL phi. A
    /// whole-word comparison cannot turn that phi back into a wave value.
    pub(super) fn check_scalar_vcc_joins(&self, lo: Value, hi: Value) -> Result<(), Why> {
        if self.phi_at.is_empty()
            || matches!(&self.x.native.io, StageIo::Cs(cs) if cs.threads == [1, 1, 1])
        {
            return Ok(());
        }
        let st = self.x.st;
        let mut pending = vec![lo];
        if self.x.native.wave64 && lo != hi {
            pending.push(hi);
        }
        let mut seen = HashSet::new();
        while let Some(value) = pending.pop() {
            if !seen.insert(value) {
                continue;
            }
            if let Some(&(b, i)) = self.phi_at.get(&value) {
                let block = &self.x.p.blocks[b];
                for (&pred, &arg) in block.preds.iter().zip(&block.phis[i].args) {
                    let edge = self.lane_edge(pred, b);
                    if !st.bools.meet(&edge) {
                        continue;
                    }
                    for (condition, _) in edge {
                        for atom in st.bools.support(condition) {
                            match atom {
                                Atom::Uniform(scc) => pending.push(scc),
                                Atom::Nonzero(low, high) => pending.extend([low, high]),
                                Atom::Any(mask) => {
                                    if !self.scalar_inputs(mask, &mut pending) {
                                        return Err(
                                            "a scalar VCC branch depending on a divergent scalar join",
                                        );
                                    }
                                }
                                _ => {
                                    return Err(
                                        "a scalar VCC branch depending on a divergent scalar join",
                                    );
                                }
                            }
                        }
                    }
                    pending.push(arg);
                }
            } else if let Some(inst) = self.def_inst(value) {
                pending.extend(inst.reads.iter().filter_map(|&(reg, input)| {
                    matches!(reg, Reg::S(_) | Reg::Scc).then_some(input)
                }));
            }
        }
        Ok(())
    }

    /// Adds the scalar values `mask`'s atoms read to `to`; false when one
    /// of them is a lane result.
    fn scalar_inputs(&self, mask: Bool, to: &mut Vec<Value>) -> bool {
        for atom in self.x.st.bools.support(mask) {
            match atom {
                Atom::Uniform(scc) => to.push(scc),
                Atom::Opaque(lo, hi) | Atom::Nonzero(lo, hi) => to.extend([lo, hi]),
                Atom::Constant(_) => {}
                _ => return false,
            }
        }
        true
    }

    /// [`Atom::Any`]: the scalar mask `mask` gives lane by lane, built as
    /// its words, has some bit set (VCC_LO alone in wave32).
    pub(super) fn any_bit(&mut self, at: usize, mask: Bool) -> Result<V, Why> {
        let (lo, hi) = self.mask_words(at, mask, &mut HashMap::new())?;
        let x = if self.x.native.wave64 {
            self.s.m.bin(at, Bin::Or, lo, hi)
        } else {
            lo
        };
        let z = self.s.m.i32(0);
        Ok(self.s.m.cmp(at, pred::NE, x, z))
    }

    /// The low and high words of the mask `b` gives lane by lane: at a
    /// node testing atom A, A's bits select the words of its high branch.
    fn mask_words(
        &mut self,
        at: usize,
        b: Bool,
        done: &mut HashMap<Bool, (V, V)>,
    ) -> Result<(V, V), Why> {
        if let Some(&w) = done.get(&b) {
            return Ok(w);
        }
        let Some((atom, low, high)) = self.x.st.bools.node(b) else {
            let w = self.s.m.i32(if b == Bool::TRUE { u32::MAX } else { 0 });
            return Ok((w, w));
        };
        let word = |w: Val| match w {
            Val::I32(x) => Ok(x),
            _ => Err("an opaque lane mask"),
        };
        let a = match atom {
            Atom::Opaque(lo, hi) => {
                self.check_scalar_vcc_joins(lo, hi)?;
                let l = word(self.get(lo)?)?;
                let h = if self.x.native.wave64 {
                    word(self.get(hi)?)?
                } else {
                    l
                };
                (l, h)
            }
            Atom::Constant(k) => (self.s.m.i32(k as u32), self.s.m.i32((k >> 32) as u32)),
            Atom::Uniform(_) | Atom::Nonzero(..) => {
                let bit = self.atom(atom, at)?;
                let (ones, z) = (self.s.m.i32(u32::MAX), self.s.m.i32(0));
                let w = self.s.m.select(at, bit, ones, z);
                (w, w)
            }
            _ => return Err("a scalar VCC built from lane results"),
        };
        let (l0, l1) = self.mask_words(at, low, done)?;
        let (h0, h1) = self.mask_words(at, high, done)?;
        // low ^ ((low ^ high) & A): high where A's bit is set, else low.
        let mut pick = |a: V, l: V, h: V| {
            let d = self.s.m.bin(at, Bin::Xor, l, h);
            let d = self.s.m.bin(at, Bin::And, d, a);
            self.s.m.bin(at, Bin::Xor, l, d)
        };
        let w = (pick(a.0, l0, h0), pick(a.1, l1, h1));
        done.insert(b, w);
        Ok(w)
    }
}
