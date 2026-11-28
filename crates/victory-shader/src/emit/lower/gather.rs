//! Gathers from a view's first level (`image_gather4_lz`, `_o`) through the
//! host's `textureGather`. Which four texels a gather takes turns on how
//! finely the sampler resolves its coordinate: the console rounds
//! `u × size` to 1/256 of a texel, half up, before taking
//! `floor(u × size − 0.5)` (`gather_first`, `gather_level_tests`), while
//! hosts differ near a texel's edge (`gather_tests`: 1/1024 of a texel below
//! one, the 780M rounds up as the console does, WARP stays below). The
//! coordinate is moved to the middle of the console's footprint, half a
//! texel from either edge, where every host takes the same four texels.

use super::Lower;
use crate::emit::bitcode::{Bin, Cast, V, pred};

impl Lower<'_> {
    /// `u` (a float) moved to `(first + 1) / size`, the middle of the
    /// footprint the console takes along an axis of `size` texels. NaN
    /// takes the footprint at 0, as on the console. Beyond ±2, where
    /// `gather_first` clamps (and a wrapping address mode would not), and
    /// at infinities, `u` is kept.
    pub(super) fn gather_console_coordinate(&mut self, u: V, size: V) -> V {
        let c = self.cur;
        let bits = self.as_int(u);
        let first = self.gather_first(bits, size);
        let one = self.s.m.i32(1);
        let middle = self.s.m.bin(c, Bin::Add, first, one);
        let f32t = self.s.t.f32;
        let middle = self.s.m.cast(c, Cast::SiToFp, middle, f32t);
        let n = self.s.m.cast(c, Cast::UiToFp, size, f32t);
        let at = self.s.m.bin(c, Bin::SDiv, middle, n);
        // FAbs; NaN compares false and takes the console's footprint.
        let magnitude = self.unary(6, u);
        let two = self.s.m.f32(2.0f32.to_bits());
        let far = self.s.m.cmp(c, pred::OGT, magnitude, two);
        self.s.m.select(c, far, u, at)
    }
}
