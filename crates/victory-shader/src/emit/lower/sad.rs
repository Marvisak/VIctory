//! Sum of absolute unsigned differences, accumulated modulo 2^32.

use super::{Attr, Bin, Lower, V};

impl Lower<'_> {
    pub(super) fn sad(&mut self, name: &str, a: V, b: V, acc: V) -> V {
        let c = self.cur;
        let width = match name {
            "v_sad_u16" => 16,
            "v_sad_u32" => 32,
            _ => 8,
        };
        let parts = 32 / width;
        let mut sum = None;
        for i in 0..parts {
            let field = |l: &mut Self, mut x| {
                if i != 0 {
                    let shift = l.s.m.i32(width * i);
                    x = l.s.m.bin(c, Bin::LShr, x, shift);
                }
                // The logical shift already zero-extends the top field.
                if i + 1 != parts {
                    let mask = l.s.m.i32((1 << width) - 1);
                    x = l.s.m.bin(c, Bin::And, x, mask);
                }
                x
            };
            let (x, y) = (field(self, a), field(self, b));
            // DXIL UMax (39) and UMin (40): an unsigned distance.
            let hi = self
                .s
                .op(c, "binary.i32", Attr::ReadNone, self.s.t.i32, 39, &[x, y]);
            let lo = self
                .s
                .op(c, "binary.i32", Attr::ReadNone, self.s.t.i32, 40, &[x, y]);
            let diff = self.s.m.bin(c, Bin::Sub, hi, lo);
            sum = Some(match sum {
                Some(sum) => self.s.m.bin(c, Bin::Add, sum, diff),
                None => diff,
            });
        }
        let mut sum = sum.expect("at least one field");
        if name == "v_sad_hi_u8" {
            let shift = self.s.m.i32(16);
            sum = self.s.m.bin(c, Bin::Shl, sum, shift);
        }
        self.s.m.bin(c, Bin::Add, sum, acc)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::lower_valu;

    #[test]
    fn sad_instruction_counts() {
        // Dynamic root constants feed all sources; a UAV store keeps the
        // result live. Counts include setup, the store and terminators.
        let mut oversized = false;
        for (op, old, expected) in [
            (0x15a, 55, 46),
            (0x15b, 56, 47),
            (0x15c, 37, 30),
            (0x15d, 24, 22),
            (0x171, 63, 63), // MSAD keeps its existing lowering (#3564).
        ] {
            let words = [0xD400_0000 | op << 16, 0x101 | 0x102 << 9 | 0x103 << 18];
            let got = lower_valu(&words).unwrap().instructions;
            eprintln!("SAD {op:#x}: {old} -> {got}, budget {expected}");
            oversized |= got > expected;
        }
        assert!(!oversized, "SAD instruction budget exceeded");
    }
}
