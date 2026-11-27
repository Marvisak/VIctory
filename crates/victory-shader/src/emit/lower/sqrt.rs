//! Binary64 square root in pairs of dwords. A 24-bit significand meets
//! the RDNA2 error bound of 2^29 binary64 ULP (ISA reference, V_SQRT_F64).

use super::{Attr, Bin, Cast, Fields, Inst, Lower, NULL, Operand, Reg, V, Why, pred};

impl Lower<'_> {
    pub(super) fn sqrt_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        if v.clamp
            || v.omod != 0
            || v.sdwa.is_some()
            || v.dpp.is_some()
            || (v.abs | v.neg) & !1 != 0
        {
            return Err("binary64 square root modifiers");
        }
        let (Some(round), Some(preserve_subnormal)) = (self.mode16.round, self.mode16.denorm_input)
        else {
            return Err("binary64 square root with unknown floating-point modes");
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a binary64 destination pair");
        };
        let dst_high = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let source = *v.src.first().ok_or("a missing binary64 source")?;
        let (low, mut high) = self.sqrt_source_words(inst, source)?;

        let c = self.cur;
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let sign_bit = m.i32(0x8000_0000);
        let magnitude_mask = m.i32(0x7fff_ffff);
        if v.abs & 1 != 0 {
            high = m.bin(c, Bin::And, high, magnitude_mask);
        }
        if v.neg & 1 != 0 {
            high = m.bin(c, Bin::Xor, high, sign_bit);
        }
        let sign = m.bin(c, Bin::And, high, sign_bit);
        let magnitude = m.bin(c, Bin::And, high, magnitude_mask);
        let shift20 = m.i32(20);
        let exponent = m.bin(c, Bin::LShr, magnitude, shift20);
        let subnormal_or_zero = m.cmp(c, pred::EQ, exponent, zero);
        let fraction_mask = m.i32(0x000f_ffff);
        let frac_high = m.bin(c, Bin::And, magnitude, fraction_mask);
        let fraction = m.bin(c, Bin::Or, low, frac_high);
        let no_fraction = m.cmp(c, pred::EQ, fraction, zero);

        let zero_operand = if preserve_subnormal {
            m.bin(c, Bin::And, subnormal_or_zero, no_fraction)
        } else {
            subnormal_or_zero
        };
        let bias = m.i32(1023);
        let unbiased = m.bin(c, Bin::Sub, exponent, bias);
        let implicit = m.i32(1 << 20);
        let mantissa_high = m.bin(c, Bin::Or, frac_high, implicit);
        let (mantissa_low, mantissa_high, unbiased) = if preserve_subnormal {
            let (lo, hi, exp) = self.normalize_sqrt_subnormal(low, frac_high);
            let m = &mut self.s.m;
            (
                m.select(c, subnormal_or_zero, lo, low),
                m.select(c, subnormal_or_zero, hi, mantissa_high),
                m.select(c, subnormal_or_zero, exp, unbiased),
            )
        } else {
            (low, mantissa_high, unbiased)
        };
        let (result_low, result_high) =
            self.approximate_sqrt_f64(mantissa_low, mantissa_high, unbiased, round);

        let m = &mut self.s.m;
        let negative = m.cmp(c, pred::NE, sign, zero);
        let max_exponent = m.i32(0x7ff);
        let is_nonfinite = m.cmp(c, pred::EQ, exponent, max_exponent);
        let nan_high = m.i32(0xfff8_0000);
        let quiet = m.i32(1 << 19);
        let quiet_high = m.bin(c, Bin::Or, high, quiet);
        let infinity_high = m.select(c, negative, nan_high, high);
        let special_high = m.select(c, no_fraction, infinity_high, quiet_high);

        let result_high = m.select(c, negative, nan_high, result_high);
        let result_high = m.select(c, zero_operand, sign, result_high);
        let result_high = m.select(c, is_nonfinite, special_high, result_high);

        let zero_low = m.bin(c, Bin::Or, negative, zero_operand);
        let result_low = m.select(c, zero_low, zero, result_low);
        let result_low = m.select(c, is_nonfinite, low, result_low);
        self.write(inst, Reg::V(reg), result_low, guard);
        self.write(inst, Reg::V(dst_high), result_high, guard);
        Ok(())
    }

    /// Move a subnormal's leading one to bit 52 and adjust the exponent.
    fn normalize_sqrt_subnormal(&mut self, low: V, high: V) -> (V, V, V) {
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

    /// For a normalized 53-bit significand M and even exponent, the first
    /// 24 root bits are floor(sqrt(M / 64)). An odd exponent doubles the
    /// radicand. Keep the discarded fraction for rounding.
    fn approximate_sqrt_f64(&mut self, low: V, high: V, exponent: V, mode: u8) -> (V, V) {
        let c = self.cur;
        let m = &mut self.s.m;
        let (zero, one, two, three) = (m.i32(0), m.i32(1), m.i32(2), m.i32(3));
        let parity = m.bin(c, Bin::And, exponent, one);
        let (six, bits32) = (m.i32(6), m.i32(32));
        let drop = m.bin(c, Bin::Sub, six, parity);
        let carry_shift = m.bin(c, Bin::Sub, bits32, drop);
        let hi = m.bin(c, Bin::LShr, high, drop);
        let lo = m.bin(c, Bin::LShr, low, drop);
        let carry = m.bin(c, Bin::Shl, high, carry_shift);
        let lo = m.bin(c, Bin::Or, lo, carry);
        let mut root = zero;
        let mut remainder = zero;

        // Bring down two bits per step. Trial (root * 4 + 1) is the
        // difference between the squares with next root bit one and zero.
        for i in (0..24).rev() {
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

        if matches!(mode, 0 | 1) {
            let mask = m.bin(c, Bin::Shl, one, drop);
            let mask = m.bin(c, Bin::Sub, mask, one);
            let fraction = m.bin(c, Bin::And, low, mask);
            let increment = if mode == 1 {
                let rem = m.bin(c, Bin::Or, remainder, fraction);
                m.cmp(c, pred::NE, rem, zero)
            } else {
                // (root + 1/2)^2 - root^2 = root + 1/4.
                let quarter_shift = m.bin(c, Bin::Sub, drop, two);
                let quarter = m.bin(c, Bin::Shl, one, quarter_shift);
                let remainder_above = m.cmp(c, pred::IUGT, remainder, root);
                let remainder_equal = m.cmp(c, pred::EQ, remainder, root);
                let fraction_above = m.cmp(c, pred::IUGT, fraction, quarter);
                let fraction_equal = m.cmp(c, pred::EQ, fraction, quarter);
                let odd = m.bin(c, Bin::And, root, one);
                let odd = m.cmp(c, pred::NE, odd, zero);
                let tie_up = m.bin(c, Bin::And, fraction_equal, odd);
                let fraction_up = m.bin(c, Bin::Or, fraction_above, tie_up);
                let at_middle = m.bin(c, Bin::And, remainder_equal, fraction_up);
                m.bin(c, Bin::Or, remainder_above, at_middle)
            };
            let increment = m.cast(c, Cast::ZExt, increment, self.s.t.i32);
            root = m.bin(c, Bin::Add, root, increment);
        }

        // The implicit root bit supplies the last bias unit. An increment
        // to 2^24 then carries naturally into the exponent above it.
        let exponent = m.bin(c, Bin::AShr, exponent, one);
        let (bias, shift20, shift29) = (m.i32(1022), m.i32(20), m.i32(29));
        let exponent = m.bin(c, Bin::Add, exponent, bias);
        let exponent = m.bin(c, Bin::Shl, exponent, shift20);
        let significand_high = m.bin(c, Bin::LShr, root, three);
        let high = m.bin(c, Bin::Add, exponent, significand_high);
        let low = m.bin(c, Bin::Shl, root, shift29);
        (low, high)
    }

    fn sqrt_source_words(&mut self, inst: &Inst, source: Operand) -> Result<(V, V), Why> {
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
