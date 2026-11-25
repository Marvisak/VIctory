//! Binary64 reciprocal square root using integer arithmetic on dwords.
//! The 28-bit square root and 26-bit quotient stay inside the RDNA2
//! accuracy bound for V_RSQ_F64 (2^29 binary64 ULP).

use super::{Attr, Bin, Cast, Fields, Inst, Lower, NULL, Operand, Reg, V, Why, pred};
use crate::emit::bitcode::Module;

impl Lower<'_> {
    pub(super) fn rsqrt_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.clamp
            || v.omod != 0
            || v.sdwa.is_some()
            || v.dpp.is_some()
            || (v.abs | v.neg) & !1 != 0
        {
            return Err("binary64 reciprocal square root modifiers");
        }
        let (Some(mode), Some(denorm)) = (self.mode16.round, self.mode16.denorm) else {
            return Err("binary64 reciprocal square root with unknown floating-point modes");
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a binary64 destination pair");
        };
        let dst_high = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let source = *v.src.first().ok_or("a missing binary64 source")?;
        let (low, mut high) = self.rsqrt_source_words(inst, source)?;

        let c = self.cur;
        let m = &mut self.s.m;
        let (zero, sign_bit, magnitude_mask) = (m.i32(0), m.i32(0x8000_0000), m.i32(0x7fff_ffff));
        if v.abs & 1 != 0 {
            high = m.bin(c, Bin::And, high, magnitude_mask);
        }
        if v.neg & 1 != 0 {
            high = m.bin(c, Bin::Xor, high, sign_bit);
        }
        let sign = m.bin(c, Bin::And, high, sign_bit);
        let magnitude = m.bin(c, Bin::And, high, magnitude_mask);
        let bits20 = m.i32(20);
        let exponent = m.bin(c, Bin::LShr, magnitude, bits20);
        let zero_exponent = m.cmp(c, pred::EQ, exponent, zero);
        let fraction_mask = m.i32(0x000f_ffff);
        let fraction_high = m.bin(c, Bin::And, magnitude, fraction_mask);
        let fraction = m.bin(c, Bin::Or, low, fraction_high);
        let no_fraction = m.cmp(c, pred::EQ, fraction, zero);
        let preserve_input = denorm & 1 != 0;
        let zero_operand = if preserve_input {
            m.bin(c, Bin::And, zero_exponent, no_fraction)
        } else {
            zero_exponent
        };

        let bias = m.i32(1023);
        let unbiased = m.bin(c, Bin::Sub, exponent, bias);
        let hidden = m.i32(1 << 20);
        let mantissa_high = m.bin(c, Bin::Or, fraction_high, hidden);
        let (mantissa_low, mantissa_high, unbiased) = if preserve_input {
            let (lo, hi, exp) = self.normalize_rsqrt_subnormal(low, fraction_high);
            let m = &mut self.s.m;
            (
                m.select(c, zero_exponent, lo, low),
                m.select(c, zero_exponent, hi, mantissa_high),
                m.select(c, zero_exponent, exp, unbiased),
            )
        } else {
            (low, mantissa_high, unbiased)
        };
        let divisor = self.rsqrt_denominator(mantissa_low, mantissa_high, unbiased, mode);
        let quotient = self.rsqrt_quotient(divisor, mode);

        // Finite nonzero results span exponents -512 through 537, so
        // binary64 output subnormals and overflow are impossible.
        let m = &mut self.s.m;
        let (one, five, bits27, exponent_bias) = (m.i32(1), m.i32(5), m.i32(27), m.i32(1021));
        let exp_half = m.bin(c, Bin::AShr, unbiased, one);
        let exp = m.bin(c, Bin::Sub, exponent_bias, exp_half);
        let exp = m.bin(c, Bin::Shl, exp, bits20);
        let frac_high = m.bin(c, Bin::LShr, quotient, five);
        let result_high = m.bin(c, Bin::Add, exp, frac_high);
        let result_low = m.bin(c, Bin::Shl, quotient, bits27);

        let negative = m.cmp(c, pred::NE, sign, zero);
        let (max_exp, infinity, invalid_nan, quiet) = (
            m.i32(0x7ff),
            m.i32(0x7ff0_0000),
            m.i32(0xfff8_0000),
            m.i32(1 << 19),
        );
        let nonfinite = m.cmp(c, pred::EQ, exponent, max_exp);
        let quiet_high = m.bin(c, Bin::Or, high, quiet);
        let infinite_input = m.select(c, negative, invalid_nan, zero);
        let special_high = m.select(c, no_fraction, infinite_input, quiet_high);
        let signed_infinity = m.bin(c, Bin::Or, sign, infinity);
        let result_high = m.select(c, negative, invalid_nan, result_high);
        let result_high = m.select(c, zero_operand, signed_infinity, result_high);
        let result_high = m.select(c, nonfinite, special_high, result_high);
        let zero_low = m.bin(c, Bin::Or, negative, zero_operand);
        let result_low = m.select(c, zero_low, zero, result_low);
        let result_low = m.select(c, nonfinite, low, result_low);
        self.write(inst, Reg::V(reg), result_low, guard);
        self.write(inst, Reg::V(dst_high), result_high, guard);
        Ok(())
    }

    /// The normalized significand M has 53 bits. The square of the
    /// 28-bit divisor is M shifted left by 2 plus the exponent's parity.
    fn rsqrt_denominator(&mut self, low: V, high: V, exponent: V, mode: u8) -> V {
        let c = self.cur;
        let m = &mut self.s.m;
        let (zero, one, two, three, bits32) = (m.i32(0), m.i32(1), m.i32(2), m.i32(3), m.i32(32));
        let parity = m.bin(c, Bin::And, exponent, one);
        let shift = m.bin(c, Bin::Add, two, parity);
        let carry_shift = m.bin(c, Bin::Sub, bits32, shift);
        let carry = m.bin(c, Bin::LShr, low, carry_shift);
        let hi = m.bin(c, Bin::Shl, high, shift);
        let hi = m.bin(c, Bin::Or, hi, carry);
        let lo = m.bin(c, Bin::Shl, low, shift);
        let mut remainder = zero;
        let mut root = zero;

        // Bring down two bits per step. Trial (root * 4 + 1) is the
        // difference between the squares with next root bit one and zero.
        for i in (0..28).rev() {
            let (word, shift) = if i >= 16 {
                (hi, 2 * (i - 16))
            } else {
                (lo, 2 * i)
            };
            let shift = m.i32(shift);
            let pair = m.bin(c, Bin::LShr, word, shift);
            let pair = m.bin(c, Bin::And, pair, three);
            let new_remainder = m.bin(c, Bin::Shl, remainder, two);
            let new_remainder = m.bin(c, Bin::Or, new_remainder, pair);
            let trial = m.bin(c, Bin::Shl, root, two);
            let trial = m.bin(c, Bin::Or, trial, one);
            let bit = m.cmp(c, pred::IUGE, new_remainder, trial);
            let difference = m.bin(c, Bin::Sub, new_remainder, trial);
            remainder = m.select(c, bit, difference, new_remainder);
            let root_shifted = m.bin(c, Bin::Shl, root, one);
            let bit = m.cast(c, Cast::ZExt, bit, self.s.t.i32);
            root = m.bin(c, Bin::Or, root_shifted, bit);
        }

        // A midpoint square differs from root^2 by root + 1/4. The
        // radicand is an integer, so exact midpoint ties cannot occur.
        // For reciprocal rounding up use the lower root. For rounding
        // down use its upper bound.
        let increment = match mode {
            0 => m.cmp(c, pred::IUGT, remainder, root),
            1 => m.bool(false),
            _ => m.cmp(c, pred::NE, remainder, zero),
        };
        let increment = m.cast(c, Cast::ZExt, increment, self.s.t.i32);
        m.bin(c, Bin::Add, root, increment)
    }

    fn rsqrt_quotient(&mut self, divisor: V, mode: u8) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let m = &mut self.s.m;
        let (zero, one) = (m.i32(0), m.i32(1));
        let away = m.bool(mode == 1);
        let numerator = m.i32(1 << 28);
        let mut remainder = m.bin(c, Bin::Sub, numerator, divisor);
        let mut quotient = one;
        for _ in 0..25 {
            let doubled = m.bin(c, Bin::Shl, remainder, one);
            let bit = m.cmp(c, pred::IUGE, doubled, divisor);
            let subtracted = m.bin(c, Bin::Sub, doubled, divisor);
            remainder = m.select(c, bit, subtracted, doubled);
            let shifted = m.bin(c, Bin::Shl, quotient, one);
            let bit = m.cast(c, Cast::ZExt, bit, i32t);
            quotient = m.bin(c, Bin::Or, shifted, bit);
        }
        let increment = if mode == 0 {
            nearest_increment(m, c, quotient, remainder, divisor)
        } else {
            let has_remainder = m.cmp(c, pred::NE, remainder, zero);
            m.bin(c, Bin::And, away, has_remainder)
        };
        let increment = m.cast(c, Cast::ZExt, increment, i32t);
        let quotient = m.bin(c, Bin::Add, quotient, increment);
        let minimum_divisor = m.i32(1 << 27);
        let quotient_is_two = m.cmp(c, pred::EQ, divisor, minimum_divisor);
        let two = m.i32(1 << 26);
        m.select(c, quotient_is_two, two, quotient)
    }

    /// Move a subnormal's leading one to bit 52 and adjust the exponent.
    fn normalize_rsqrt_subnormal(&mut self, low: V, high: V) -> (V, V, V) {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let leading_low = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 33, &[low]);
        let leading_high = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 33, &[high]);
        let m = &mut self.s.m;
        let (zero, bits31, bits32) = (m.i32(0), m.i32(31), m.i32(32));
        let low_shift_base = m.i32(21); // 52 - 31
        let high_shift_base = m.i32((-11i32) as u32); // 52 - 63
        let low_shift = m.bin(c, Bin::Add, leading_low, low_shift_base);
        let high_shift = m.bin(c, Bin::Add, leading_high, high_shift_base);
        let high_nonzero = m.cmp(c, pred::NE, high, zero);
        let shift = m.select(c, high_nonzero, high_shift, low_shift);
        let short_shift = m.bin(c, Bin::And, shift, bits31);
        let carry_shift = m.bin(c, Bin::Sub, bits32, short_shift);
        let carry_shift = m.bin(c, Bin::And, carry_shift, bits31);
        let carry = m.bin(c, Bin::LShr, low, carry_shift);
        let short_nonzero = m.cmp(c, pred::NE, short_shift, zero);
        let carry = m.select(c, short_nonzero, carry, zero);
        let shifted_high = m.bin(c, Bin::Shl, high, short_shift);
        let shifted_high = m.bin(c, Bin::Or, shifted_high, carry);
        let shifted_low = m.bin(c, Bin::Shl, low, short_shift);
        let crosses_dword = m.cmp(c, pred::IUGE, shift, bits32);
        let high = m.select(c, crosses_dword, shifted_low, shifted_high);
        let low = m.select(c, crosses_dword, zero, shifted_low);
        let min_exponent = m.i32((-1022i32) as u32);
        let exponent = m.bin(c, Bin::Sub, min_exponent, shift);
        (low, high, exponent)
    }

    fn rsqrt_source_words(&mut self, inst: &Inst, source: Operand) -> Result<(V, V), Why> {
        let second = |reg: u8| reg.checked_add(1).ok_or("a missing binary64 high word");
        let pair = |l: &mut Self, high| {
            let high = inst.read(high).ok_or("an unread binary64 high word")?;
            Ok((l.operand(inst, &source)?, l.i32_of(high)?))
        };
        match source {
            Operand::Vector { reg, .. } => pair(self, Reg::V(second(reg)?)),
            Operand::Scalar { code, .. } if code != NULL => pair(self, Reg::S(second(code)?)),
            Operand::Literal(high) => Ok((self.s.m.i32(0), self.s.m.i32(high))),
            Operand::Const(code @ 240..=248) => {
                let bits = if code == 248 {
                    0x3fc4_5f30_6dc9_c882 // Binary64 1/(2 pi).
                } else {
                    let value = crate::resource::inline(code).ok_or("an unknown inline float")?;
                    f64::from(f32::from_bits(value)).to_bits()
                };
                Ok((self.s.m.i32(bits as u32), self.s.m.i32((bits >> 32) as u32)))
            }
            _ => {
                let high = if matches!(source, Operand::Const(193..=208)) {
                    u32::MAX
                } else {
                    0
                };
                Ok((self.operand(inst, &source)?, self.s.m.i32(high)))
            }
        }
    }
}

// Compare a discarded fraction to one half and break exact ties to even.
fn nearest_increment(m: &mut Module, c: usize, value: V, residue: V, denominator: V) -> V {
    let (zero, one) = (m.i32(0), m.i32(1));
    let doubled = m.bin(c, Bin::Shl, residue, one);
    let above = m.cmp(c, pred::IUGT, doubled, denominator);
    let equal = m.cmp(c, pred::EQ, doubled, denominator);
    let odd = m.bin(c, Bin::And, value, one);
    let odd = m.cmp(c, pred::NE, odd, zero);
    let tie_up = m.bin(c, Bin::And, equal, odd);
    m.bin(c, Bin::Or, above, tie_up)
}
