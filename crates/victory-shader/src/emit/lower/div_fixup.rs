use super::{Bin, Fields, Inst, Lower, Operand, V, Why, pred};
use crate::ir::Reg;

const SIGN: u32 = 0x8000_0000;
const INFINITY: u32 = 0x7ff0_0000;
const ONE: u32 = 0x3ff0_0000;

struct Classification {
    zero: V,
    infinity: V,
    nan: V,
}

impl Lower<'_> {
    /// RDNA2 adjusts the supplied quotient using the original operands.
    /// The modeled mode preserves f64 denormals and incoming sNaNs.
    /// Measurements: AnyPS5 #474, #610, #7041 and #19563.
    pub(super) fn div_fixup_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a double result outside a VGPR pair");
        };
        let hi_reg = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let [q_lo, q_hi] = self.div_fixup_source(inst, 0)?;
        let den = self.div_fixup_source(inst, 1)?;
        let num = self.div_fixup_source(inst, 2)?;
        let d = self.classify_fixup_source(den);
        let n = self.classify_fixup_source(num);
        let c = self.cur;
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let one = m.i32(1);
        let infinity = m.i32(INFINITY);
        let sign_mask = m.i32(SIGN);
        let mag_mask = m.i32(!SIGN);
        let signs = m.bin(c, Bin::Xor, den[1], num[1]);
        let sign = m.bin(c, Bin::And, signs, sign_mask);
        let negative = m.cmp(c, pred::NE, sign, zero);
        let positive = m.cmp(c, pred::EQ, sign, zero);

        let den_exp = m.bin(c, Bin::And, den[1], infinity);
        let num_exp = m.bin(c, Bin::And, num[1], infinity);
        let exp_delta = m.bin(c, Bin::Sub, num_exp, den_exp);
        // Difference of the two exponent fields, each already shifted by 20.
        let min_delta = m.i32((-1075i32 << 20) as u32);
        let too_small = m.cmp(c, pred::SLT, exp_delta, min_delta);
        let q_exp = m.bin(c, Bin::And, q_hi, infinity);
        let overflow = m.cmp(c, pred::EQ, q_exp, infinity);
        let words = m.bin(c, Bin::And, q_hi, mag_mask);
        let q_signed = [q_lo, m.bin(c, Bin::Or, words, sign)];

        // Directed overflow saturates only when rounding this sign inward.
        // Directed underflow may instead return the smallest subnormal.
        let (saturate, tiny_nonzero) = match self.mode16 {
            Some(1) => (negative, positive),
            Some(2) => (positive, negative),
            Some(3) => (m.bool(true), m.bool(false)),
            _ => (m.bool(false), m.bool(false)),
        };
        let max_lo = m.i32(u32::MAX);
        let max_hi = m.i32(INFINITY - 1);
        let overflow_lo = m.select(c, saturate, max_lo, zero);
        let overflow_hi = m.select(c, saturate, max_hi, infinity);
        let overflow_pair = [overflow_lo, m.bin(c, Bin::Or, overflow_hi, sign)];
        let min_lo = m.select(c, tiny_nonzero, one, zero);
        let signed_inf = [zero, m.bin(c, Bin::Or, infinity, sign)];
        let invalid = [zero, m.i32(0xfff8_0000)];
        let signed_zero = [zero, sign];

        let zeros = m.bin(c, Bin::And, d.zero, n.zero);
        let infinities = m.bin(c, Bin::And, d.infinity, n.infinity);
        let invalid_inputs = m.bin(c, Bin::Or, zeros, infinities);
        let yields_zero = m.bin(c, Bin::Or, d.infinity, n.zero);
        let yields_inf = m.bin(c, Bin::Or, d.zero, n.infinity);
        // Apply overrides from lowest priority to highest.
        let mut result = q_signed;
        for (cond, selected) in [
            (overflow, overflow_pair),
            (too_small, [min_lo, sign]),
            (yields_zero, signed_zero),
            (yields_inf, signed_inf),
            (invalid_inputs, invalid),
            (d.nan, den),
            (n.nan, num),
        ] {
            result = [
                m.select(c, cond, selected[0], result[0]),
                m.select(c, cond, selected[1], result[1]),
            ];
        }

        // RDNA2 ignores f64 omod. DX10 clamp maps negative values and NaNs
        // to +0, leaving positive subnormals and values in [0,1] intact.
        if v.clamp {
            let one = m.i32(ONE);
            let below_one = m.cmp(c, pred::IULT, result[1], one);
            let lo_zero = m.cmp(c, pred::EQ, result[0], zero);
            let at_inf_exp = m.cmp(c, pred::EQ, result[1], infinity);
            let positive_inf = m.bin(c, Bin::And, lo_zero, at_inf_exp);
            let positive_finite = m.cmp(c, pred::IULT, result[1], infinity);
            let nonnegative = m.bin(c, Bin::Or, positive_finite, positive_inf);
            let lo = m.select(c, below_one, result[0], zero);
            let hi = m.select(c, below_one, result[1], one);
            result = [
                m.select(c, nonnegative, lo, zero),
                m.select(c, nonnegative, hi, zero),
            ];
        }
        self.write(inst, Reg::V(reg), result[0], guard);
        self.write(inst, Reg::V(hi_reg), result[1], guard);
        Ok(())
    }

    fn div_fixup_source(&mut self, inst: &Inst, k: usize) -> Result<[V; 2], Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let (lo, mut hi) = match op {
            Operand::Literal(word) => (self.s.m.i32(0), self.s.m.i32(word)),
            Operand::Const(code @ 240..=248) => {
                let bits = if code == 248 {
                    0x3fc4_5f30_6dc9_c882 // RDNA2 ISA, instruction operands table
                } else {
                    let bits = crate::resource::inline(code).ok_or("an unknown inline float")?;
                    f64::from(f32::from_bits(bits)).to_bits()
                };
                (self.s.m.i32(bits as u32), self.s.m.i32((bits >> 32) as u32))
            }
            _ => self.operand64_dwords(inst, op)?,
        };
        let c = self.cur;
        if v.abs >> k & 1 != 0 {
            let mask = self.s.m.i32(!SIGN);
            hi = self.s.m.bin(c, Bin::And, hi, mask);
        }
        if v.neg >> k & 1 != 0 {
            let sign = self.s.m.i32(SIGN);
            hi = self.s.m.bin(c, Bin::Xor, hi, sign);
        }
        Ok([lo, hi])
    }

    fn classify_fixup_source(&mut self, [lo, hi]: [V; 2]) -> Classification {
        let c = self.cur;
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let infinity = m.i32(INFINITY);
        let mag_mask = m.i32(!SIGN);
        let mag_hi = m.bin(c, Bin::And, hi, mag_mask);
        let combined = m.bin(c, Bin::Or, mag_hi, lo);
        let would_be_infinite = m.cmp(c, pred::EQ, mag_hi, infinity);
        let lo_empty = m.cmp(c, pred::EQ, lo, zero);
        let lo_payload = m.cmp(c, pred::NE, lo, zero);
        let nan_hi = m.cmp(c, pred::IUGT, mag_hi, infinity);
        let nan_lo = m.bin(c, Bin::And, would_be_infinite, lo_payload);
        Classification {
            zero: m.cmp(c, pred::EQ, combined, zero),
            infinity: m.bin(c, Bin::And, would_be_infinite, lo_empty),
            nan: m.bin(c, Bin::Or, nan_hi, nan_lo),
        }
    }
}
