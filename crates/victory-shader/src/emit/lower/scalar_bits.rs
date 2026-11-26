//! Scalar bit counts and scans of a 64-bit pair that have no DXIL operation
//! of their own: `s_bcnt0_i32_b64`, `s_ff0_i32_b64`, `s_flbit_i32_b64` and
//! `s_flbit_i32_i64`. Each is built from the 32-bit `unaryBits` operations
//! (31 count bits, 32 first bit low, 33 first bit high, 34 first bit high
//! signed) on the two dwords. Source: the RDNA2 ISA guide (SOP1).

use super::Lower;
use crate::emit::bitcode::{Attr, Bin, V, pred};

impl Lower<'_> {
    /// The result of `name` on the pair `hi:lo`, and the SCC value for the
    /// instruction that sets one.
    pub(super) fn pair_bits(&mut self, name: &str, lo: V, hi: V) -> (V, Option<V>) {
        let c = self.cur;
        let bits = |l: &mut Self, op, v| {
            l.s.op(c, "unaryBits.i32", Attr::ReadNone, l.s.t.i32, op, &[v])
        };
        let (z, k32, m1) = (self.s.m.i32(0), self.s.m.i32(32), self.s.m.i32(u32::MAX));
        if name.starts_with("s_flbit") {
            // The first bit from the top of the pair (for the signed form,
            // the first that differs from the sign), counted from bit 63; -1
            // for none. The high dword answers unless it has no such bit;
            // then it is all sign (or zero), and the low dword is searched
            // for a bit that differs from it.
            let signed = name == "s_flbit_i32_i64";
            let fh = bits(self, if signed { 34 } else { 33 }, hi);
            let lo = if signed {
                self.s.m.bin(c, Bin::Xor, lo, hi)
            } else {
                lo
            };
            let fl = bits(self, 33, lo);
            let fl32 = self.s.m.bin(c, Bin::Add, fl, k32);
            let none = self.s.m.cmp(c, pred::EQ, lo, z);
            let fl = self.s.m.select(c, none, m1, fl32);
            let high = self.s.m.cmp(c, pred::NE, fh, m1);
            return (self.s.m.select(c, high, fh, fl), None);
        }
        // The zero forms are the one forms of the complement.
        let lo = self.s.m.bin(c, Bin::Xor, lo, m1);
        let hi = self.s.m.bin(c, Bin::Xor, hi, m1);
        if name == "s_bcnt0_i32_b64" {
            // The count of clear bits; SCC = result != 0.
            let (nl, nh) = (bits(self, 31, lo), bits(self, 31, hi));
            let r = self.s.m.bin(c, Bin::Add, nl, nh);
            let s = self.s.m.cmp(c, pred::NE, r, z);
            return (r, Some(s));
        }
        // s_ff0_i32_b64: the lowest clear bit, -1 for none, as s_ff1_i32_b64.
        let (fl, fh) = (bits(self, 32, lo), bits(self, 32, hi));
        let fh32 = self.s.m.bin(c, Bin::Add, fh, k32);
        let none = self.s.m.cmp(c, pred::EQ, hi, z);
        let fh = self.s.m.select(c, none, fh, fh32);
        let low = self.s.m.cmp(c, pred::NE, lo, z);
        (self.s.m.select(c, low, fl, fh), None)
    }
}
