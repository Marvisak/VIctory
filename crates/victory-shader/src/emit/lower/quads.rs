//! Compute samples that take an implicit level: the console's lane quads.

use super::{Attr, Bin, Lower, V, Val, Why, flags};

/// An image instruction that takes its level from the coordinates'
/// derivatives: a sample without an explicit level (`_l`, `_lz`) or
/// gradients (`_d`, `_cd`), and `image_get_lod`.
pub(super) fn implicit_level(name: &str) -> bool {
    name == "image_get_lod"
        || name.strip_prefix("image_sample").is_some_and(|rest| {
            !rest
                .split('_')
                .any(|t| matches!(t, "l" | "lz" | "d" | "cd"))
        })
}

impl Lower<'_> {
    /// Component `c` of the thread id in a console group of `[x, y, z]`
    /// threads run as one host row: taken back from the flat id.
    pub(super) fn row_thread_id(&mut self, [x, y, _]: [u32; 3], c: u8) -> Val {
        let e = self.s.entry;
        let f = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            self.s.t.i32,
            96,
            &[],
        );
        let (xk, yk, xy) = (self.s.m.i32(x), self.s.m.i32(y), self.s.m.i32(x * y));
        Val::I32(match c {
            0 => self.s.m.bin(e, Bin::URem, f, xk),
            1 => {
                let t = self.s.m.bin(e, Bin::UDiv, f, xk);
                self.s.m.bin(e, Bin::URem, t, yk)
            }
            _ => self.s.m.bin(e, Bin::UDiv, f, xy),
        })
    }

    /// The x then y gradients (three each, undefined past `coords`) of
    /// float coordinates `coords` across this lane's quad: lane 1 less lane
    /// 0, lane 2 less lane 0, as the hardware's coarse derivatives; each
    /// times `scale` if given.
    pub(super) fn quad_gradients(&mut self, coords: &[V], scale: Option<V>) -> Vec<V> {
        self.s.flags |= flags::WAVE_OPS;
        let c = self.cur;
        let fu = self.s.m.undef(self.s.t.f32);
        let lane = self.lane_index();
        let (not3, one, two) = (self.s.m.i32(!3), self.s.m.i32(1), self.s.m.i32(2));
        let base = self.s.m.bin(c, Bin::And, lane, not3);
        let right = self.s.m.bin(c, Bin::Or, base, one);
        let below = self.s.m.bin(c, Bin::Or, base, two);
        let mut g = vec![fu; 6];
        for (k, &x) in coords.iter().enumerate().take(3) {
            let xi = self.as_int(x);
            let at = |l: &mut Self, lane: V| {
                let v = l.read_lane(xi, lane);
                l.as_float(v)
            };
            let (x0, x1, x2) = (at(self, base), at(self, right), at(self, below));
            g[k] = self.s.m.bin(c, Bin::Sub, x1, x0);
            g[3 + k] = self.s.m.bin(c, Bin::Sub, x2, x0);
            if let Some(s) = scale {
                g[k] = self.s.m.bin(c, Bin::Mul, g[k], s);
                g[3 + k] = self.s.m.bin(c, Bin::Mul, g[3 + k], s);
            }
        }
        g
    }

    /// A sample's texel offsets from its offset word `w`, into `os`: DXIL
    /// takes immediate offsets in -8..=7; the console's are 6-bit and may
    /// be computed.
    pub(super) fn sample_offsets(&mut self, w: V, os: &mut [V]) -> Result<(), Why> {
        let w = self
            .s
            .m
            .int_value(w)
            .ok_or("a sample offset that is not constant")? as u32;
        for (k, o) in os.iter_mut().enumerate() {
            let x = (w >> (8 * k) & 0x3F) as i32;
            let x = if x >= 32 { x - 64 } else { x };
            if !(-8..=7).contains(&x) {
                return Err("a sample offset past -8..7");
            }
            *o = self.s.m.i32(x as u32);
        }
        Ok(())
    }
}
