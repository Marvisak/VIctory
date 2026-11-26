//! Format-dependent half-precision conversion of image reads.

use super::super::bitcode::{Attr, Bin, Cast, Term, V, pred};
use super::super::shader::{
    VIEW_NORMALIZED, VIEW_SINT, VIEW_SNORM16, VIEW_TEXTURE_MASK, VIEW_UINT, VIEW_UNORM16,
};
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
        let index = *self
            .s
            .view_index
            .get(&h)
            .ok_or("an image without a heap index")?;
        let entry = self.view_word(index);
        Ok(self.pack_image_d16(entry, channels, texel, point_choice))
    }

    fn pack_image_d16(
        &mut self,
        entry: V,
        channels: &[V],
        texel: bool,
        point_choice: Option<V>,
    ) -> Vec<V> {
        let c = self.cur;
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
        self.end_guarded(norm, &pairs)
    }

    /// Loads the same heap slot through a handle of the selected component
    /// type. Only the matching branch may access the texture.
    pub(super) fn image_load_d16(
        &mut self,
        h: V,
        kind: u32,
        args: &mut [V],
        channels: &[u32],
    ) -> Result<Vec<V>, Why> {
        let index = *self
            .s
            .view_index
            .get(&h)
            .ok_or("an image without a heap index")?;
        let entry = self.view_word(index);
        let mask = self.s.m.i32(VIEW_TEXTURE_MASK);
        let tag = self.s.m.bin(self.cur, Bin::And, entry, mask);
        let uint = self.s.m.i32(VIEW_UINT);
        let sint = self.s.m.i32(VIEW_SINT);
        let is_uint = self.s.m.cmp(self.cur, pred::EQ, tag, uint);
        let is_sint = self.s.m.cmp(self.cur, pred::EQ, tag, sint);
        let [unsigned, check_signed, signed, float, join] =
            std::array::from_fn(|_| self.s.m.new_block());
        self.s
            .m
            .terminate(self.cur, Term::CondBr(is_uint, unsigned, check_signed));
        self.s
            .m
            .terminate(check_signed, Term::CondBr(is_sint, signed, float));

        self.cur = float;
        let r = self.s.op(
            float,
            "textureLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            66,
            args,
        );
        let values: Vec<_> = channels
            .iter()
            .map(|&ch| self.s.m.extract(float, r, ch))
            .collect();
        let floats = self.pack_image_d16(entry, &values, true, None);
        let float_end = self.cur;
        self.s.m.terminate(float_end, Term::Br(join));

        // Match the rasterizer ordering of the original UAV declaration.
        let kind = if self.pops && kind & 0x1000 != 0 {
            kind | 0x2000
        } else {
            kind
        };
        let ret = self
            .s
            .m
            .named_struct("dx.types.ResRet.i32", vec![self.s.t.i32; 5]);
        let mut load_integer = |block, signed| {
            let component = if signed { 4 } else { 5 }; // DXIL I32 / U32
            args[0] = self
                .s
                .heap_handle(block, index, false, true, (kind, 0x400 | component));
            let r = self
                .s
                .op(block, "textureLoad.i32", Attr::ReadOnly, ret, 66, args);
            let mask = self.s.m.i32(0xffff);
            let top = self.s.m.i32(if signed { 0x7fff } else { 0xffff });
            channels
                .iter()
                .map(|&ch| {
                    let x = self.s.m.extract(block, r, ch);
                    let over =
                        self.s
                            .m
                            .cmp(block, if signed { pred::SGT } else { pred::IUGT }, x, top);
                    let x = self.s.m.select(block, over, top, x);
                    if signed {
                        let bottom = self.s.m.i32((-32768i32) as u32);
                        let under = self.s.m.cmp(block, pred::SLT, x, bottom);
                        let x = self.s.m.select(block, under, bottom, x);
                        self.s.m.bin(block, Bin::And, x, mask)
                    } else {
                        x
                    }
                })
                .collect::<Vec<_>>()
        };
        let uints = load_integer(unsigned, false);
        let sints = load_integer(signed, true);
        self.s.m.terminate(unsigned, Term::Br(join));
        self.s.m.terminate(signed, Term::Br(join));

        self.cur = join;
        Ok(floats
            .into_iter()
            .zip(uints)
            .zip(sints)
            .map(|((f, u), s)| {
                let phi = self.s.m.phi(join, self.s.t.i32);
                self.s
                    .m
                    .set_phi(phi, vec![(f, float_end), (u, unsigned), (s, signed)]);
                phi
            })
            .collect())
    }
}
