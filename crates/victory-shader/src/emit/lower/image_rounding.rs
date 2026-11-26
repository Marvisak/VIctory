//! Format-dependent half-precision conversion of image reads.

use super::super::bitcode::{Attr, Bin, Cast, V, pred};
use super::super::shader::{VIEW_NORMALIZED, VIEW_SNORM16, VIEW_UNORM16};
use super::{Lower, Why};

impl<'a> Lower<'a> {
    /// A 16-bit normalized texel has an odd denominator, 65535 or 32767.
    /// Recover its integer magnitude and round that exact ratio once.
    fn image_norm16(&mut self, x: V, signed: V) -> V {
        let c = self.cur;
        let u = self.s.m.f32(65535.0f32.to_bits());
        let s = self.s.m.f32(32767.0f32.to_bits());
        let max = self.s.m.select(c, signed, s, u);
        let a = self.unary(6, x);
        let scaled = self.s.m.bin(c, Bin::Mul, a, max);
        let mag = self.unary(26, scaled);
        let mag = self.s.m.cast(c, Cast::FpToUi, mag, self.s.t.i32);
        // The binary expansion repeats the 16 or 15 magnitude bits.
        // Keep the first 32 fraction bits (all ones represents 1 here).
        let (u, s, k13) = (self.s.m.i32(65537), self.s.m.i32(131076), self.s.m.i32(13));
        let rep = self.s.m.select(c, signed, s, u);
        let fixed = self.s.m.bin(c, Bin::Mul, mag, rep);
        let more = self.s.m.bin(c, Bin::LShr, mag, k13);
        let zero = self.s.m.i32(0);
        let more = self.s.m.select(c, signed, more, zero);
        let fixed = self.s.m.bin(c, Bin::Add, fixed, more);
        let leading = self.s.op(
            c,
            "unaryBits.i32",
            Attr::ReadNone,
            self.s.t.i32,
            33,
            &[fixed],
        );
        // Bit 31 represents 2^-1. Shift at most 13 places to the f16
        // normal threshold (2^-14), leaving subnormals in one binade.
        // FirstbitHi returns -1 for zero, so the unsigned clamp handles it.
        let is_subnormal = self.s.m.cmp(c, pred::IUGT, leading, k13);
        let shift = self.s.m.select(c, is_subnormal, k13, leading);
        let mantissa = self.s.m.bin(c, Bin::Shl, fixed, shift);
        // Keep the leading bit, ten fraction bits and the round bit.
        let k20 = self.s.m.i32(20);
        let doubled = self.s.m.bin(c, Bin::LShr, mantissa, k20);
        let one = self.s.m.i32(1);
        // A nonzero, noninteger ratio cannot equal a binary midpoint.
        let biased = self.s.m.bin(c, Bin::Add, doubled, one);
        let rounded = self.s.m.bin(c, Bin::LShr, biased, one);
        let k10 = self.s.m.i32(10);
        let exp = self.s.m.bin(c, Bin::Sub, k13, shift);
        let exp = self.s.m.bin(c, Bin::Shl, exp, k10);
        let half = self.s.m.bin(c, Bin::Add, exp, rounded);
        let bits = self.as_int(x);
        let k16 = self.s.m.i32(16);
        let sign = self.s.m.bin(c, Bin::LShr, bits, k16);
        let sign_bit = self.s.m.i32(0x8000);
        let sign = self.s.m.bin(c, Bin::And, sign, sign_bit);
        self.s.m.bin(c, Bin::Or, sign, half)
    }

    pub(super) fn image_d16(
        &mut self,
        h: V,
        channels: &[V],
        texel: bool,
        point_choice: Option<V>,
    ) -> Result<Vec<V>, Why> {
        let c = self.cur;
        let index = *self
            .s
            .view_index
            .get(&h)
            .ok_or("an image without a heap index")?;
        let entry = self.view_word(index);
        let flag = self.s.m.i32(VIEW_NORMALIZED);
        let raw = self.s.m.bin(c, Bin::And, entry, flag);
        let normalized = self.s.m.cmp(c, pred::EQ, raw, flag);
        // `legacyF32ToF16` alone truncates on Windows but rounds to nearest
        // under vkd3d-proton (NVIDIA).
        let truncate: Vec<_> = channels
            .iter()
            .map(|&x| self.f16_toward_zero(x))
            .collect();
        let norm = self.begin_guarded(Some(normalized));
        let nearest: Vec<_> = channels
            .iter()
            .map(|&x| self.f32_to_f16(x, false))
            .collect();
        let nearest = if texel || point_choice.is_some() {
            // Loads and gathers return texels. A filtered sample can be
            // between two codes and must keep that interpolation.
            let wide = self.s.m.i32(VIEW_UNORM16);
            let raw = self.s.m.bin(self.cur, Bin::And, entry, wide);
            let wide = self.s.m.cmp(self.cur, pred::EQ, raw, wide);
            let wide = if let Some(point) = point_choice {
                self.s.m.bin(self.cur, Bin::And, wide, point)
            } else {
                wide
            };
            let signed = self.s.m.i32(VIEW_SNORM16);
            let raw = self.s.m.bin(self.cur, Bin::And, entry, signed);
            let signed = self.s.m.cmp(self.cur, pred::EQ, raw, signed);
            let at = self.begin_guarded(Some(wide));
            let exact: Vec<_> = channels
                .iter()
                .map(|&x| self.image_norm16(x, signed))
                .zip(nearest)
                .collect();
            self.end_guarded(at, &exact)
        } else {
            nearest
        };
        let pairs: Vec<_> = nearest.into_iter().zip(truncate).collect();
        Ok(self.end_guarded(norm, &pairs))
    }
}
