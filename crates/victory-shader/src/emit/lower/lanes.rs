//! Compute reading lanes only within aligned groups narrower than the
//! console's wave (`Lowered::min_lanes`): the group each instruction needs,
//! and thread ids and console lanes taken from the host's own waves.

use super::{Lower, Val, reads_mask_as_data};
use crate::emit::bitcode::{Attr, Bin, V};
use crate::emit::shader::flags;
use crate::gfx10::{Dpp, Fields};
use crate::ir::Inst;
use crate::structure::Structured;

/// The aligned group of lanes whose values `i` reads or whose layout it
/// needs (`wave`: the whole wave; 0: none).
pub(super) fn lane_group(st: &Structured, i: &Inst, wave: u32) -> u32 {
    let n = i.name();
    if reads_mask_as_data(st, i) {
        return wave;
    }
    match &i.fields {
        Fields::Valu(v) if v.dpp.is_some() => match v.dpp {
            Some(Dpp::Lanes { .. }) => 8,
            Some(Dpp::Row { ctrl: 0..=0xFF, .. }) => 4,
            // GFX10 has no row broadcasts or wave shifts: these stay in a row.
            Some(Dpp::Row {
                ctrl: 0x101..=0x10F | 0x111..=0x11F | 0x121..=0x12F | 0x140 | 0x141 | 0x150..=0x16F,
                ..
            }) => 16,
            _ => wave,
        },
        Fields::Ds {
            offset0, offset1, ..
        } if n == "ds_swizzle_b32" => {
            let offset = u32::from(*offset0) | u32::from(*offset1) << 8;
            if offset & 0x8000 != 0 {
                return 4;
            }
            // The lane bits the and, or and xor masks change.
            let bits = !offset & 0x1F | offset >> 5 & 0x1F | offset >> 10 & 0x1F;
            1 << (32 - bits.leading_zeros())
        }
        _ if n.starts_with("ds_swizzle")
            || n.contains("permute")
            || n.starts_with("v_mbcnt")
            || matches!(n, "v_readlane_b32" | "v_writelane_b32") =>
        {
            wave
        }
        _ => 0,
    }
}

impl Lower<'_> {
    /// [`Lower::thread_id`] in compute declaring no wave size.
    pub(super) fn narrow_thread_id(&mut self, c: u8) -> Val {
        let [x, y, _] = self.s.threads.expect("compute");
        let e = self.s.entry;
        let f = self.lane_thread();
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

    /// Compute declaring no wave size: the flat thread id relabelled as in
    /// [`Lower::thread_id`], rounded to the host's wave size. A console
    /// lane's aligned group is then a host lane's as long as host waves hold
    /// whole groups ([`Lowered::min_lanes`]).
    fn lane_thread(&mut self) -> V {
        if let Some(f) = self.lane_thread {
            return f;
        }
        let e = self.s.entry;
        let i32t = self.s.t.i32;
        let flat = self.s.op(
            e,
            "flattenedThreadIdInGroup.i32",
            Attr::ReadNone,
            i32t,
            96,
            &[],
        );
        let first = self.s.op(
            e,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[flat],
        );
        let n = self
            .s
            .op(e, "waveGetLaneCount", Attr::ReadNone, i32t, 112, &[]);
        let z = self.s.m.i32(0);
        let mask = self.s.m.bin(e, Bin::Sub, z, n);
        let base = self.s.m.bin(e, Bin::And, first, mask);
        let cur = std::mem::replace(&mut self.cur, e);
        let lane = self.lane_index();
        self.cur = cur;
        self.s.flags |= flags::WAVE_OPS;
        let f = self.s.m.bin(e, Bin::Add, base, lane);
        self.lane_thread = Some(f);
        f
    }

    /// The console lane of host lane `lane` in compute declaring no wave
    /// size: its relabelled thread's place in the console wave.
    pub(super) fn console_lane(&mut self, lane: V) -> V {
        if self.min_lanes == 0 {
            return lane;
        }
        let f = self.lane_thread();
        let m = self.s.m.i32(self.wave() - 1);
        self.s.m.bin(self.s.entry, Bin::And, f, m)
    }
}
