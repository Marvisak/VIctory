//! Binary64 reciprocal using a 28-bit divisor and 26-bit quotient in
//! dwords, within the RDNA2 limit of 2^29 binary64 ULP (V_RCP_F64).

use super::{Attr, Bin, Cast, Fields, Inst, Lower, NULL, Operand, Reg, V, Why, pred};
use crate::emit::bitcode::Module;

impl Lower<'_> {
    pub(super) fn reciprocal_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.clamp
            || v.omod != 0
            || v.sdwa.is_some()
            || v.dpp.is_some()
            || (v.abs | v.neg) & !1 != 0
        {
            return Err("binary64 reciprocal modifiers");
        }
        let (Some(mode), Some(denorm)) = (self.mode16.round, self.mode16.denorm) else {
            return Err("binary64 reciprocal with unknown floating-point modes");
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a binary64 destination pair");
        };
        let dst_high = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let source = *v.src.first().ok_or("a missing binary64 source")?;
        let (low, mut high) = self.reciprocal_source_words(inst, source)?;

        let c = self.cur;
        let i32t = self.s.t.i32;
        let m = &mut self.s.m;
        let (zero, bits20) = (m.i32(0), m.i32(20));
        let (sign_bit, magnitude_mask) = (m.i32(0x8000_0000), m.i32(0x7fff_ffff));
        if v.abs & 1 != 0 {
            high = m.bin(c, Bin::And, high, magnitude_mask);
        }
        if v.neg & 1 != 0 {
            high = m.bin(c, Bin::Xor, high, sign_bit);
        }
        let sign = m.bin(c, Bin::And, high, sign_bit);
        let away = match mode {
            1 => m.cmp(c, pred::EQ, sign, zero),
            2 => m.cmp(c, pred::NE, sign, zero),
            _ => m.bool(false),
        };
        let magnitude = m.bin(c, Bin::And, high, magnitude_mask);
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
            let (lo, hi, exp) = self.normalize_reciprocal_subnormal(low, fraction_high);
            let m = &mut self.s.m;
            (
                m.select(c, zero_exponent, lo, low),
                m.select(c, zero_exponent, hi, mantissa_high),
                m.select(c, zero_exponent, exp, unbiased),
            )
        } else {
            (low, mantissa_high, unbiased)
        };
        let quotient = self.reciprocal_significand(mantissa_low, mantissa_high, away, mode);

        let m = &mut self.s.m;
        // The reciprocal significand is in [1, 2]. For an exact power of
        // two it is 2, carrying one more into the true output exponent.
        let minus_one = m.i32(u32::MAX);
        let base_exponent = m.bin(c, Bin::Sub, minus_one, unbiased);
        let high_is_hidden = m.cmp(c, pred::EQ, mantissa_high, hidden);
        let low_is_zero = m.cmp(c, pred::EQ, mantissa_low, zero);
        let exact_power = m.bin(c, Bin::And, high_is_hidden, low_is_zero);
        let exponent_carry = m.cast(c, Cast::ZExt, exact_power, i32t);
        let exact_exponent = m.bin(c, Bin::Add, base_exponent, exponent_carry);

        let (min_normal, bias_minus_one) = (m.i32((-1022i32) as u32), m.i32(1022));
        let encode_subnormal = m.cmp(c, pred::SLT, base_exponent, min_normal);
        let normal_exponent = m.bin(c, Bin::Add, base_exponent, bias_minus_one);
        let normal_exponent = m.bin(c, Bin::Shl, normal_exponent, bits20);
        let (five, bits27, bits32) = (m.i32(5), m.i32(27), m.i32(32));
        let quotient_high = m.bin(c, Bin::LShr, quotient, five);
        let normal_high = m.bin(c, Bin::Add, normal_exponent, quotient_high);
        let normal_low = m.bin(c, Bin::Shl, quotient, bits27);

        // Subnormal reciprocals of finite binary64 values shift by at most
        // two extra bits. The selected shift stays in range on both paths.
        let shift_bias = m.i32(1049); // 1022 + 27
        let shift = m.bin(c, Bin::Add, base_exponent, shift_bias);
        let shift = m.select(c, encode_subnormal, shift, bits27);
        let carry_shift = m.bin(c, Bin::Sub, bits32, shift);
        let subnormal_low = m.bin(c, Bin::Shl, quotient, shift);
        let subnormal_high = m.bin(c, Bin::LShr, quotient, carry_shift);
        let result_low = m.select(c, encode_subnormal, subnormal_low, normal_low);
        let result_high = m.select(c, encode_subnormal, subnormal_high, normal_high);

        // An approximate quotient can carry past MAX even for a mathematically
        // finite reciprocal. Use the exact exponent for overflow behavior.
        let (max_low, max_high, infinity) =
            (m.i32(u32::MAX), m.i32(0x7fef_ffff), m.i32(0x7ff0_0000));
        let coarse_overflow = m.cmp(c, pred::IUGE, result_high, infinity);
        let finite_low = m.select(c, coarse_overflow, max_low, result_low);
        let finite_high = m.select(c, coarse_overflow, max_high, result_high);
        let actual_overflow = m.cmp(c, pred::SGT, exact_exponent, bias);
        let overflow_is_infinite = if mode == 0 { m.bool(true) } else { away };
        let overflow_low = m.select(c, overflow_is_infinite, zero, max_low);
        let overflow_high = m.select(c, overflow_is_infinite, infinity, max_high);
        let mut result_low = m.select(c, actual_overflow, overflow_low, finite_low);
        let mut result_high = m.select(c, actual_overflow, overflow_high, finite_high);
        if denorm & 2 == 0 {
            let flush = m.cmp(c, pred::SLT, exact_exponent, min_normal);
            result_low = m.select(c, flush, zero, result_low);
            result_high = m.select(c, flush, zero, result_high);
        }

        let result_low = m.select(c, zero_operand, zero, result_low);
        let result_high = m.select(c, zero_operand, infinity, result_high);
        let infinity_exponent = m.i32(0x7ff);
        let nonfinite = m.cmp(c, pred::EQ, exponent, infinity_exponent);
        let quiet_bit = m.i32(1 << 19);
        let quiet_high = m.bin(c, Bin::Or, magnitude, quiet_bit);
        let special_high = m.select(c, no_fraction, zero, quiet_high);
        let result_low = m.select(c, nonfinite, low, result_low);
        let result_high = m.select(c, nonfinite, special_high, result_high);
        let result_high = m.bin(c, Bin::Or, result_high, sign);
        self.write(inst, Reg::V(reg), result_low, guard);
        self.write(inst, Reg::V(dst_high), result_high, guard);
        Ok(())
    }

    fn reciprocal_significand(&mut self, low: V, high: V, away: V, mode: u8) -> V {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let m = &mut self.s.m;
        let (zero, one, bits7, bits25) = (m.i32(0), m.i32(1), m.i32(7), m.i32(25));
        let high = m.bin(c, Bin::Shl, high, bits7);
        let top_low = m.bin(c, Bin::LShr, low, bits25);
        let divisor = m.bin(c, Bin::Or, high, top_low);
        let fraction_mask = m.i32((1 << 25) - 1);
        let fraction = m.bin(c, Bin::And, low, fraction_mask);

        // Bound the divisor on the opposite side for directed rounding.
        // The discarded source fraction moves the quotient less than half
        // a 26-bit step, leaving room for the final directed increment.
        let fraction_denominator = m.i32(1 << 25);
        let divisor_increment = if mode == 0 {
            nearest_increment(m, c, divisor, fraction, fraction_denominator)
        } else {
            let has_fraction = m.cmp(c, pred::NE, fraction, zero);
            let zero_increment = m.bool(false);
            m.select(c, away, zero_increment, has_fraction)
        };
        let divisor_increment = m.cast(c, Cast::ZExt, divisor_increment, i32t);
        let divisor = m.bin(c, Bin::Add, divisor, divisor_increment);
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
    fn normalize_reciprocal_subnormal(&mut self, low: V, high: V) -> (V, V, V) {
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

    fn reciprocal_source_words(&mut self, inst: &Inst, source: Operand) -> Result<(V, V), Why> {
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
