use super::{Attr, Bin, Cast, Fields, Inst, Lower, Operand, V, Why, pred};
use crate::emit::bitcode::Module;
use crate::ir::Reg;

const SIGN: u32 = 0x8000_0000;
const FRACTION: u32 = 0x000f_ffff;
const INFINITY: u32 = 0x7ff0_0000;
const ONE: u32 = 0x3ff0_0000;
const STEP: u32 = 128 << 20;

impl Lower<'_> {
    /// RDNA2 division pre-scaling and the accompanying lane mask.
    /// See AnyPS5 #474 and #24629 for the measured thresholds.
    pub(super) fn div_scale_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a double result outside a VGPR pair");
        };
        let high = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let value = self.div_scale_argument(inst, 0)?;
        let denominator = self.div_scale_argument(inst, 1)?;
        let numerator = self.div_scale_argument(inst, 2)?;
        let c = self.cur;
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let up = m.i32(1);
        let down = m.i32(u32::MAX);
        let de = exponent(m, c, denominator[1]);
        let ne = exponent(m, c, numerator[1]);
        let delta = m.bin(c, Bin::Sub, ne, de);
        let above_ratio = m.i32(768);
        let below_ratio = m.i32(-768i32 as u32);
        let large_ratio = m.cmp(c, pred::SGE, delta, above_ratio);
        let small_ratio = m.cmp(c, pred::SLE, delta, below_ratio);
        let not_small_ratio = m.cmp(c, pred::SGT, delta, below_ratio);
        let den_zero_exp = m.cmp(c, pred::EQ, de, zero);
        let den_normal_exp = m.cmp(c, pred::NE, de, zero);
        let huge = m.i32(2045);
        let tiny = m.i32(53);
        let huge_den = m.cmp(c, pred::IUGE, de, huge);
        let tiny_num = m.cmp(c, pred::IULE, ne, tiny);
        let equal_low = m.cmp(c, pred::EQ, value[0], denominator[0]);
        let equal_high = m.cmp(c, pred::EQ, value[1], denominator[1]);
        let use_denominator = m.bin(c, Bin::And, equal_low, equal_high);

        // The signed decision is -1, 0, or +1 in exponent steps of 128.
        let mild = m.select(c, tiny_num, up, zero);
        let small = m.select(c, use_denominator, zero, up);
        let scale = m.select(c, small_ratio, small, mild);
        let wants_down = m.bin(c, Bin::Or, use_denominator, not_small_ratio);
        let huge_scale = m.select(c, wants_down, down, zero);
        let scale = m.select(c, huge_den, huge_scale, scale);
        let scale = m.select(c, den_zero_exp, up, scale);
        let large = m.select(c, use_denominator, up, zero);
        let scale = m.select(c, large_ratio, large, scale);
        let flag_small = m.bin(c, Bin::And, den_normal_exp, small_ratio);
        let flag = m.bin(c, Bin::Or, large_ratio, flag_small);
        let den_zero = is_zero(m, c, denominator);
        let num_zero = is_zero(m, c, numerator);
        let invalid = m.bin(c, Bin::Or, den_zero, num_zero);

        let mut result = self.div_scale_words(value, scale);
        let m = &mut self.s.m;
        let invalid_high = m.i32(0xfff8_0000);
        result = select_words(m, c, invalid, [zero, invalid_high], result);
        if v.clamp {
            result = clamp_words(m, c, result);
        }
        // As measured on RDNA2 (#663), f64 ignores omod.
        self.write(inst, Reg::V(reg), result[0], guard);
        self.write(inst, Reg::V(high), result[1], guard);
        if let Some(Operand::Scalar { code, .. }) = v.carry
            && let Some(w) = inst.write(Reg::S(code))
        {
            self.lane_bits.insert(w, flag);
        }
        for &(_, w) in &inst.writes {
            self.vals
                .entry(w)
                .or_insert(super::Val::Missing("a lane mask read as data"));
        }
        Ok(())
    }

    fn div_scale_argument(&mut self, inst: &Inst, k: usize) -> Result<[V; 2], Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let op = *v.src.get(k).ok_or("a missing operand")?;
        let (lo, mut hi) = match op {
            Operand::Literal(high) => (self.s.m.i32(0), self.s.m.i32(high)),
            Operand::Const(code @ 240..=248) => {
                let bits = if code == 248 {
                    0x3fc4_5f30_6dc9_c882 // RDNA2's inline binary64 constant.
                } else {
                    let bits = crate::resource::inline(code).ok_or("an unknown inline float")?;
                    f64::from(f32::from_bits(bits)).to_bits()
                };
                (self.s.m.i32(bits as u32), self.s.m.i32((bits >> 32) as u32))
            }
            _ => self.operand64_dwords(inst, op)?,
        };
        let c = self.cur;
        if v.neg >> k & 1 != 0 {
            let sign_mask = self.s.m.i32(SIGN);
            hi = self.s.m.bin(c, Bin::Xor, hi, sign_mask);
        }
        Ok([lo, hi])
    }

    /// Scale by an exact power of two in the modeled f64 mode, which keeps
    /// denormals. RDNA2 treats a downscaled exponent below 117 as overflow.
    fn div_scale_words(&mut self, [lo, hi]: [V; 2], scale: V) -> [V; 2] {
        let c = self.cur;
        let round = self.mode16.unwrap_or(0);
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let sign_mask = m.i32(SIGN);
        let mag_mask = m.i32(!SIGN);
        let frac_mask = m.i32(FRACTION);
        let sign = m.bin(c, Bin::And, hi, sign_mask);
        let magnitude = m.bin(c, Bin::And, hi, mag_mask);
        let fraction = m.bin(c, Bin::And, hi, frac_mask);
        let exp = exponent(m, c, hi);
        let is_denormal = m.cmp(c, pred::EQ, exp, zero);
        let round_away = match round {
            1 => m.cmp(c, pred::EQ, sign, zero),
            2 => m.cmp(c, pred::NE, sign, zero),
            _ => m.bool(false),
        };
        let over_to_inf = if round == 0 { m.bool(true) } else { round_away };
        let max_lo = m.i32(u32::MAX);
        let max_hi = m.i32(INFINITY - 1);
        let inf_hi = m.i32(INFINITY);
        let overflow = [
            m.select(c, over_to_inf, zero, max_lo),
            m.select(c, over_to_inf, inf_hi, max_hi),
        ];
        let step = m.i32(STEP);

        let up_hi = m.bin(c, Bin::Add, magnitude, step);
        let too_high = m.i32(2047 - 128);
        let over = m.cmp(c, pred::IUGE, exp, too_high);
        let up_normal = select_words(m, c, over, overflow, [lo, up_hi]);
        let up_subnormal = self.normalize_div_scale_subnormal(lo, fraction);
        let m = &mut self.s.m;
        let scaled_up = select_words(m, c, is_denormal, up_subnormal, up_normal);

        let first_normal = m.i32(129);
        let first_subnormal = m.i32(117);
        let down_normal = m.cmp(c, pred::IUGE, exp, first_normal);
        let down_overflow = m.cmp(c, pred::IULT, exp, first_subnormal);
        let down_hi = m.bin(c, Bin::Sub, magnitude, step);
        let outside_subnormal = m.bin(c, Bin::Or, down_normal, down_overflow);
        let amount = m.bin(c, Bin::Sub, first_normal, exp);
        let one = m.i32(1);
        let amount = m.select(c, outside_subnormal, one, amount);
        let down_subnormal =
            round_div_scale_subnormal(m, c, lo, fraction, amount, round == 0, round_away);
        let scaled_down = select_words(m, c, down_normal, [lo, down_hi], down_subnormal);
        let scaled_down = select_words(m, c, down_overflow, overflow, scaled_down);

        let scale_positive = m.cmp(c, pred::SGT, scale, zero);
        let mut result = select_words(m, c, scale_positive, scaled_up, scaled_down);
        result[1] = m.bin(c, Bin::Or, result[1], sign);
        let nonfinite_exp = m.i32(2047);
        let nonfinite = m.cmp(c, pred::EQ, exp, nonfinite_exp);
        let same_scale = m.cmp(c, pred::EQ, scale, zero);
        let zero_value = is_zero(m, c, [lo, hi]);
        let keep = m.bin(c, Bin::Or, nonfinite, same_scale);
        let keep = m.bin(c, Bin::Or, keep, zero_value);
        select_words(m, c, keep, [lo, hi], result)
    }

    fn normalize_div_scale_subnormal(&mut self, lo: V, fraction: V) -> [V; 2] {
        let c = self.cur;
        let i32t = self.s.t.i32;
        let low_leading = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 33, &[lo]);
        let high_leading = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 33, &[fraction]);
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let hi_base = m.i32(-11i32 as u32);
        let lo_base = m.i32(21);
        let hi_shift = m.bin(c, Bin::Add, high_leading, hi_base);
        let lo_shift = m.bin(c, Bin::Add, low_leading, lo_base);
        let has_high = m.cmp(c, pred::NE, fraction, zero);
        let shift = m.select(c, has_high, hi_shift, lo_shift);

        let k31 = m.i32(31);
        let k32 = m.i32(32);
        let inner = m.bin(c, Bin::And, shift, k31);
        let one_word = m.cmp(c, pred::IUGE, shift, k32);
        let lo_shifted = m.bin(c, Bin::Shl, lo, inner);
        let hi_shifted = m.bin(c, Bin::Shl, fraction, inner);
        let complement = m.bin(c, Bin::Sub, zero, inner);
        let complement = m.bin(c, Bin::And, complement, k31);
        let carry = m.bin(c, Bin::LShr, lo, complement);
        let aligned = m.cmp(c, pred::EQ, inner, zero);
        let carry = m.select(c, aligned, zero, carry);
        let joined = m.bin(c, Bin::Or, hi_shifted, carry);
        let lo = m.select(c, one_word, zero, lo_shifted);
        let hi = m.select(c, one_word, lo_shifted, joined);

        let frac_mask = m.i32(FRACTION);
        let fraction = m.bin(c, Bin::And, hi, frac_mask);
        let bias = m.i32(129);
        let exp = m.bin(c, Bin::Sub, bias, shift);
        let shift20 = m.i32(20);
        let field = m.bin(c, Bin::Shl, exp, shift20);
        [lo, m.bin(c, Bin::Or, field, fraction)]
    }
}

fn exponent(m: &mut Module, c: usize, hi: V) -> V {
    let shift = m.i32(20);
    let mask = m.i32(0x7ff);
    let field = m.bin(c, Bin::LShr, hi, shift);
    m.bin(c, Bin::And, field, mask)
}

fn is_zero(m: &mut Module, c: usize, [lo, hi]: [V; 2]) -> V {
    let mask = m.i32(!SIGN);
    let hi = m.bin(c, Bin::And, hi, mask);
    let either = m.bin(c, Bin::Or, lo, hi);
    let zero = m.i32(0);
    m.cmp(c, pred::EQ, either, zero)
}

fn select_words(m: &mut Module, c: usize, cond: V, a: [V; 2], b: [V; 2]) -> [V; 2] {
    [m.select(c, cond, a[0], b[0]), m.select(c, cond, a[1], b[1])]
}

/// The downscale produces a denormal only for exponents 117 through 128,
/// so the right shift is 1 through 12 and all discarded bits are in lo.
fn round_div_scale_subnormal(
    m: &mut Module,
    c: usize,
    lo: V,
    fraction: V,
    shift: V,
    nearest: bool,
    away: V,
) -> [V; 2] {
    let zero = m.i32(0);
    let one = m.i32(1);
    let hidden = m.i32(0x0010_0000);
    let hi = m.bin(c, Bin::Or, fraction, hidden);
    let k32 = m.i32(32);
    let complement = m.bin(c, Bin::Sub, k32, shift);
    let cross = m.bin(c, Bin::Shl, hi, complement);
    let shifted_lo = m.bin(c, Bin::LShr, lo, shift);
    let shifted_lo = m.bin(c, Bin::Or, cross, shifted_lo);
    let shifted_hi = m.bin(c, Bin::LShr, hi, shift);
    let unit = m.bin(c, Bin::Shl, one, shift);
    let mask = m.bin(c, Bin::Sub, unit, one);
    let lost = m.bin(c, Bin::And, lo, mask);
    let round_up = if nearest {
        let halfway = m.bin(c, Bin::LShr, unit, one);
        let over = m.cmp(c, pred::IUGT, lost, halfway);
        let tie = m.cmp(c, pred::EQ, lost, halfway);
        let lsb = m.bin(c, Bin::And, shifted_lo, one);
        let odd = m.cmp(c, pred::NE, lsb, zero);
        let tie_up = m.bin(c, Bin::And, tie, odd);
        m.bin(c, Bin::Or, over, tie_up)
    } else {
        let inexact = m.cmp(c, pred::NE, lost, zero);
        m.bin(c, Bin::And, away, inexact)
    };
    let i32t = m.int(32);
    let step = m.cast(c, Cast::ZExt, round_up, i32t);
    let rounded = m.bin(c, Bin::Add, shifted_lo, step);
    let carry = m.cmp(c, pred::IULT, rounded, shifted_lo);
    let carry = m.cast(c, Cast::ZExt, carry, i32t);
    [rounded, m.bin(c, Bin::Add, shifted_hi, carry)]
}

fn clamp_words(m: &mut Module, c: usize, value: [V; 2]) -> [V; 2] {
    let zero = m.i32(0);
    let one = m.i32(ONE);
    let inf = m.i32(INFINITY);
    let below_one = m.cmp(c, pred::IULT, value[1], one);
    let at_inf = m.cmp(c, pred::EQ, value[1], inf);
    let no_payload = m.cmp(c, pred::EQ, value[0], zero);
    let infinity = m.bin(c, Bin::And, at_inf, no_payload);
    let positive_finite = m.cmp(c, pred::IULT, value[1], inf);
    let keeps_value = m.bin(c, Bin::Or, positive_finite, infinity);
    let capped = select_words(m, c, below_one, value, [zero, one]);
    select_words(m, c, keeps_value, capped, [zero, zero])
}
