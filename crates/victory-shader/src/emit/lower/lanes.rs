//! Lane masks of wave32 pixel and vertex programs on hosts that run 64
//! lanes per wave (see `MaskData::Host`).

use super::{Lower, MaskData};
use crate::emit::bitcode::{Attr, Bin, V, pred};
use crate::emit::shader::flags;

impl Lower<'_> {
    /// For a wave32 program with host lanes (pixel and vertex), whether
    /// this lane is in the upper half of a 64-lane host wave: such a host
    /// runs two console waves' worth of lanes in one, so a 32-bit lane mask
    /// is the ballot's dword of the lane's half, and lane numbers from masks
    /// (`s_ff1`, `v_mbcnt`) count within the half. Never true on a host
    /// running 32 lanes. `None` where masks are not split so.
    pub(super) fn host_half(&mut self) -> Option<V> {
        if self.mask_data != MaskData::Host || self.x.native.wave64 {
            return None;
        }
        self.s.flags |= flags::WAVE_OPS;
        let lane = self.s.op(
            self.cur,
            "waveGetLaneIndex",
            Attr::ReadOnly,
            self.s.t.i32,
            111,
            &[],
        );
        let k32 = self.s.m.i32(32);
        Some(self.s.m.cmp(self.cur, pred::IUGE, lane, k32))
    }

    /// The host lane of `lane` counted within this lane's half (see
    /// [`Lower::host_half`]): `lane` itself where masks are not split.
    pub(super) fn host_lane(&mut self, lane: V) -> V {
        let Some(upper) = self.host_half() else {
            return lane;
        };
        let (k32, z) = (self.s.m.i32(32), self.s.m.i32(0));
        let base = self.s.m.select(self.cur, upper, k32, z);
        self.s.m.bin(self.cur, Bin::Add, lane, base)
    }

    /// `v_readfirstlane`: `x` of the first active lane. With masks split
    /// by halves (see [`Lower::host_half`]), the first active lane of this
    /// lane's half: a 64-lane host wave holds two console waves, and
    /// `waveReadLaneFirst` over the whole host wave gave lanes 32-63 a value
    /// from lanes 0-31.
    pub(super) fn read_first_lane(&mut self, x: V) -> V {
        self.s.flags |= flags::WAVE_OPS;
        let i32t = self.s.t.i32;
        let Some(upper) = self.host_half() else {
            let c = self.cur;
            return self
                .s
                .op(c, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[x]);
        };
        let c = self.cur;
        let four = self.s.m.named_struct("dx.types.fouri32", vec![i32t; 4]);
        let on = self.s.m.bool(true);
        let ballot = self
            .s
            .op(c, "waveActiveBallot", Attr::NoUnwind, four, 116, &[on]);
        let (lo, hi) = (
            self.s.m.extract(c, ballot, 0),
            self.s.m.extract(c, ballot, 1),
        );
        let half = self.s.m.select(c, upper, hi, lo);
        // FirstbitLo of a dword holding this lane's own bit (a helper lane,
        // left out of the ballot before 6.7, reads its own: helper_reads_own).
        let first = self
            .s
            .op(c, "unaryBits.i32", Attr::ReadNone, i32t, 32, &[half]);
        let (k32, z) = (self.s.m.i32(32), self.s.m.i32(0));
        let base = self.s.m.select(c, upper, k32, z);
        let lane = self.s.m.bin(c, Bin::Add, first, base);
        self.read_lane(x, lane)
    }
}
