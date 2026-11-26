use super::{Attr, Bin, Fields, Inst, Lower, Operand, V, Why, pred};
use crate::emit::bitcode::Module;
use crate::ir::Reg;

/// The first 1184 fractional bits of 2/pi, most significant word first.
/// The final zero is the tail past the measured RDNA2 table (AnyPS5 #447).
const TWO_OVER_PI: [u32; 38] = [
    0xa2f9_836e,
    0x4e44_1529,
    0xfc27_57d1,
    0xf534_ddc0,
    0xdb62_9599,
    0x3c43_9041,
    0xfe51_63ab,
    0xdebb_c561,
    0xb724_6e3a,
    0x424d_d2e0,
    0x0649_2eea,
    0x09d1_921c,
    0xfe1d_eb1c,
    0xb129_a73e,
    0xe882_35f5,
    0x2ebb_4484,
    0xe99c_7026,
    0xb45f_7e41,
    0x3991_d639,
    0x8353_39f4,
    0x9c84_5f8b,
    0xbdf9_283b,
    0x1ff8_97ff,
    0xde05_980f,
    0xef2f_118b,
    0x5a0a_6d1f,
    0x6d36_7ecf,
    0x27cb_09b7,
    0x4f46_3f66,
    0x9e5f_ea2d,
    0x7527_bac7,
    0xebe5_f17b,
    0x3d07_39f7,
    0x8a52_92ea,
    0x6bfb_5fb1,
    0x1f8d_5d08,
    0x5603_3046,
    0,
];

impl Lower<'_> {
    pub(super) fn trig_preop_f64(&mut self, inst: &Inst, guard: Option<V>) -> Result<(), Why> {
        let Fields::Valu(v) = &inst.fields else {
            unreachable!()
        };
        let Some(Operand::Vector { reg, .. }) = v.dst else {
            return Err("a double result outside a VGPR pair");
        };
        let high = reg
            .checked_add(1)
            .ok_or("a missing destination high word")?;
        let op = *v.src.first().ok_or("a missing operand")?;
        // Only the exponent matters. All inline numbers are below the first
        // shift threshold, and input sign modifiers do not change it.
        let input_high = match op {
            Operand::Literal(high) => self.s.m.i32(high),
            Operand::Const(128..=208 | 240..=248) => self.s.m.i32(0),
            _ => self.operand64_dwords(inst, op)?.1,
        };
        let selector = self.operand(inst, v.src.get(1).ok_or("a missing selector")?)?;
        let table = self.trig_preop_table();
        let c = self.cur;
        let m = &mut self.s.m;

        let shift20 = m.i32(20);
        let exp_mask = m.i32(0x7ff);
        let exp = m.bin(c, Bin::LShr, input_high, shift20);
        let exp = m.bin(c, Bin::And, exp, exp_mask);
        let threshold = m.i32(1077);
        let past_threshold = m.cmp(c, pred::IUGT, exp, threshold);
        let extra = m.bin(c, Bin::Sub, exp, threshold);
        let zero = m.i32(0);
        let extra = m.select(c, past_threshold, extra, zero);
        let mask = m.i32(31);
        let width = m.i32(53);
        let segment = m.bin(c, Bin::And, selector, mask);
        let start = m.bin(c, Bin::Mul, segment, width);
        let start = m.bin(c, Bin::Add, start, extra);

        let mantissa = read_window(m, c, table, start);
        let large_exp = m.i32(1968);
        let large = m.cmp(c, pred::IUGE, exp, large_exp);
        let boost = m.i32(128);
        let boost = m.select(c, large, boost, zero);
        // Largest possible exponent of a window scaled by 2^(-53 - start).
        let biased = m.i32(1022);
        let biased = m.bin(c, Bin::Sub, biased, start);
        let biased = m.bin(c, Bin::Add, biased, boost);
        let [lo, hi] = self.trig_preop_pack(mantissa, biased);
        // The result is positive and below one, so clamp does not affect it.
        // f64 OMOD is ignored with denormals kept, the model used here.
        self.write(inst, Reg::V(reg), lo, guard);
        self.write(inst, Reg::V(high), hi, guard);
        Ok(())
    }

    fn trig_preop_table(&mut self) -> V {
        if let Some(table) = self.trig_preop_table {
            return table;
        }
        let m = &mut self.s.m;
        let words: Vec<_> = TWO_OVER_PI.iter().map(|&word| m.i32(word)).collect();
        let ty = m.array(words.len() as u32, self.s.t.i32);
        let init = m.aggregate(ty, &words);
        let table = m.constant_global("trig_preop_two_over_pi", init);
        self.trig_preop_table = Some(table);
        table
    }

    fn trig_preop_pack(&mut self, [lo, hi]: [V; 2], biased: V) -> [V; 2] {
        let c = self.cur;
        let ty = self.s.t.i32;
        let low_leading = self.s.op(c, "unaryBits.i32", Attr::ReadNone, ty, 33, &[lo]);
        let high_leading = self.s.op(c, "unaryBits.i32", Attr::ReadNone, ty, 33, &[hi]);
        let m = &mut self.s.m;
        let zero = m.i32(0);
        let hi_base = m.i32(-11_i32 as u32);
        let lo_base = m.i32(21);
        let shift_hi = m.bin(c, Bin::Add, high_leading, hi_base);
        let shift_lo = m.bin(c, Bin::Add, low_leading, lo_base);
        let has_hi = m.cmp(c, pred::NE, hi, zero);
        let leading = m.select(c, has_hi, shift_hi, shift_lo);
        let bits = m.bin(c, Bin::Or, hi, lo);
        let nonzero = m.cmp(c, pred::NE, bits, zero);

        let biased = m.bin(c, Bin::Sub, biased, leading);
        let is_normal = m.cmp(c, pred::SGT, biased, zero);
        let [low, high] = normalize(m, c, [lo, hi], leading);
        let fraction = m.i32(0x000f_ffff);
        let fraction = m.bin(c, Bin::And, high, fraction);
        let shift20 = m.i32(20);
        let exp = m.bin(c, Bin::Shl, biased, shift20);
        let normal_high = m.bin(c, Bin::Or, exp, fraction);

        // Below the normal range, keep the integral subnormal bits and
        // discard the rest. This opcode always rounds toward zero.
        let one = m.i32(1);
        let subnormal_shift = m.bin(c, Bin::Sub, one, biased);
        let [sub_low, sub_high] = truncate(m, c, [low, high], subnormal_shift);
        let result_lo = m.select(c, is_normal, low, sub_low);
        let result_hi = m.select(c, is_normal, normal_high, sub_high);
        [
            m.select(c, nonzero, result_lo, zero),
            m.select(c, nonzero, result_hi, zero),
        ]
    }
}

fn read_window(m: &mut Module, c: usize, table: V, start: V) -> [V; 2] {
    let five = m.i32(5);
    let index = m.bin(c, Bin::LShr, start, five);
    let end = m.i32((TWO_OVER_PI.len() - 1) as u32);
    let [first, second, third] = [0, 1, 2].map(|word| {
        let offset = m.i32(word);
        let index = m.bin(c, Bin::Add, index, offset);
        let past_end = m.cmp(c, pred::IUGT, index, end);
        let index = m.select(c, past_end, end, index);
        let address = m.element(c, table, index);
        m.load(c, address)
    });
    let mask = m.i32(31);
    let inner = m.bin(c, Bin::And, start, mask);
    let upper = funnel_left(m, c, first, second, inner);
    let lower = funnel_left(m, c, second, third, inner);
    let remove = m.i32(11);
    let carry = m.i32(21);
    let hi = m.bin(c, Bin::LShr, upper, remove);
    let upper_lo = m.bin(c, Bin::Shl, upper, carry);
    let lower_lo = m.bin(c, Bin::LShr, lower, remove);
    [m.bin(c, Bin::Or, upper_lo, lower_lo), hi]
}

/// Align two adjacent words by a count in 0..32, keeping the leading word.
fn funnel_left(m: &mut Module, c: usize, lead: V, following: V, inner: V) -> V {
    let zero = m.i32(0);
    let mask = m.i32(31);
    let complement = m.bin(c, Bin::Sub, zero, inner);
    let complement = m.bin(c, Bin::And, complement, mask);
    let cross = m.bin(c, Bin::LShr, following, complement);
    let aligned = m.cmp(c, pred::EQ, inner, zero);
    let cross = m.select(c, aligned, zero, cross);
    let shifted = m.bin(c, Bin::Shl, lead, inner);
    m.bin(c, Bin::Or, shifted, cross)
}

/// Left shift a nonzero 53-bit window to put its leading bit at bit 52.
fn normalize(m: &mut Module, c: usize, [lo, hi]: [V; 2], leading: V) -> [V; 2] {
    let zero = m.i32(0);
    let mask = m.i32(31);
    let word = m.i32(32);
    let inner = m.bin(c, Bin::And, leading, mask);
    let crosses_word = m.cmp(c, pred::IUGE, leading, word);
    let shifted_lo = m.bin(c, Bin::Shl, lo, inner);
    let shifted_hi = funnel_left(m, c, hi, lo, inner);
    [
        m.select(c, crosses_word, zero, shifted_lo),
        m.select(c, crosses_word, shifted_lo, shifted_hi),
    ]
}

/// Shift the normalized window right, returning zero past its last bit.
fn truncate(m: &mut Module, c: usize, [lo, hi]: [V; 2], count: V) -> [V; 2] {
    let zero = m.i32(0);
    let mask = m.i32(31);
    let word = m.i32(32);
    let two_words = m.i32(64);
    let inner = m.bin(c, Bin::And, count, mask);
    let complement = m.bin(c, Bin::Sub, zero, inner);
    let complement = m.bin(c, Bin::And, complement, mask);
    let cross = m.bin(c, Bin::Shl, hi, complement);
    let aligned = m.cmp(c, pred::EQ, inner, zero);
    let cross = m.select(c, aligned, zero, cross);
    let shifted_lo = m.bin(c, Bin::LShr, lo, inner);
    let shifted_lo = m.bin(c, Bin::Or, shifted_lo, cross);
    let shifted_hi = m.bin(c, Bin::LShr, hi, inner);
    let crosses_word = m.cmp(c, pred::IUGE, count, word);
    let past_end = m.cmp(c, pred::IUGE, count, two_words);
    let low = m.select(c, crosses_word, shifted_hi, shifted_lo);
    [
        m.select(c, past_end, zero, low),
        m.select(c, crosses_word, zero, shifted_hi),
    ]
}
