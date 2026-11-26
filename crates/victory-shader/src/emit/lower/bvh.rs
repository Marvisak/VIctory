//! `image_bvh_intersect_ray` and `image_bvh64_intersect_ray` emulated
//! (decision 0005): one ray against one node of a BVH the console wrote, the
//! result in the hardware's encoding, so the program's own traversal runs
//! unchanged. Sources: the RDNA2 ISA guide ("Ray Tracing": operands, the
//! descriptor, return modes), AMD's GPURT (`IntersectCommon.hlsl`, its
//! software form of the instruction; node layouts in `BoxNode1_0.hlsli`,
//! `TriangleNode1_0.hlsli`, `Common.hlsl`) and Mesa RADV (`bvh.h`,
//! `radv_rt_common.c`: node structures, software box and triangle tests).
//!
//! * Address VGPRs, f32 unless named: node pointer (two dwords, low first, in
//!   the 64-bit form), ray extent, origin xyz, direction xyz, inverse
//!   direction xyz. (`a16` packs both directions as f16: refused.)
//! * Descriptor, 128 bits: base address over 256 in bits 0-39, box growth in
//!   ULPs 55-62, box sorting 63, node count minus 1 (a bound) 64-105, the
//!   triangle return mode 120 (1: barycentrics, 0: triangle id and hit
//!   status), a page-size hint 123, type 8 in 124-127. Programs build it in
//!   code; `resource` accepts box sorting, no growth, no bound and
//!   barycentrics (the page-size hint either way), what is emulated here.
//! * The node is at `base * 256 + (pointer & !7) * 8`; the pointer's low
//!   three bits are its type: 0-3 triangle, 4 box16, 5 box32, 6 instance, 7
//!   procedural. Instance and procedural nodes return four `0xffffffff`.
//! * Box nodes: child pointers in dwords 0-3, then four boxes, as f32 min xyz
//!   and max xyz (box32, 128 bytes) or f16 pairs `minx|miny, minz|maxx,
//!   maxy|maxz` (box16, 64 bytes). Per box and axis the planes give
//!   `(plane - origin) * inverse`; the near one (the min plane where the
//!   inverse is >= 0) bounds tmin, the far one tmax; `tmin = max(nears, 0)`,
//!   `tmax = min(fars, extent)`, a hit when `tmin <= tmax`. Hits keep their
//!   pointer, misses (and a NaN bound) give `0xffffffff`; the four are
//!   sorted by tmin with GPURT's network ((0,2), (1,3), (0,1), (2,3),
//!   (1,2)), misses last, equal distances in network order. Conservative
//!   where the sources differ: a ray lying in a plane (direction 0, origin on
//!   the plane: `0 * inf`) is not bounded by it, and the extent is inclusive.
//! * Triangle nodes, 64 bytes: vertices V0-V4 of three f32 from byte 0 (V4 at
//!   48, over the id fields of a node with fewer), the triangle id at 60.
//!   Type t tests (V0 V1 V2), (V1 V3 V2), (V2 V3 V4) or (V2 V4 V0) (GPURT's
//!   vertex table; its own builder writes types 0 and 1 only). Result: t's
//!   numerator and denominator, then the barycentric numerators that bits
//!   `8t..8t+1` (I) and `8t+2..8t+3` (J) of the triangle id pick (0 the first
//!   vertex's weight, 1 the second's, 2 the third's; 3, undefined, reads 0);
//!   RADV's nodes hold 9 there, the plain order. A positive denominator is a
//!   front face: the ray meets `(V1 - V0) x (V2 - V0)` head on. The test is
//!   watertight (Woop, Benthin and Wald, JCGT 2013, as RADV's): edges and
//!   vertices count as inside, a zero-area triangle and anything NaN miss,
//!   and a miss is `(+inf, 1, 0, 0)`. No extent test: programs compare t.
//!
//! The numerators' scale is the emulation's own (the hardware's is not
//! public); programs use the ratios and the denominator's sign.

use super::{Lower, Why};
use crate::emit::bitcode::{Attr, Bin, Cast, Term, V, pred};
use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Reg};
use crate::resource::Source;

const INVALID: u32 = u32::MAX;

/// Where a site reads its nodes.
enum Nodes {
    /// A raw buffer and the node's byte offset in it.
    Buffer(V, V),
    /// The node's 64-bit GPU address.
    Raw(V),
}

/// A ray: extent, origin, direction, inverse direction (f32).
struct Ray {
    extent: V,
    origin: [V; 3],
    dir: [V; 3],
    inv: [V; 3],
}

impl Lower<'_> {
    pub(super) fn bvh(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let Fields::Image {
            data,
            addr,
            rsrc,
            a16,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        if *a16 {
            return Err("a BVH intersection with 16-bit directions");
        }
        let wide = inst.name() == "image_bvh64_intersect_ray";
        let mut regs = Vec::new();
        for a in addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        let word = |l: &mut Self, k: usize| -> Result<V, Why> {
            let v = regs
                .get(k)
                .and_then(|&r| inst.read(Reg::V(r)))
                .ok_or("a BVH intersection operand")?;
            l.i32_of(v)
        };
        let node = word(self, 0)?;
        let node_hi = if wide { Some(word(self, 1)?) } else { None };
        let first = 1 + usize::from(wide);
        let mut f = [self.s.m.f32(0); 10];
        for (k, x) in f.iter_mut().enumerate() {
            let w = word(self, first + k)?;
            *x = self.as_float(w);
        }
        let ray = Ray {
            extent: f[0],
            origin: [f[1], f[2], f[3]],
            dir: [f[4], f[5], f[6]],
            inv: [f[7], f[8], f[9]],
        };
        let Operand::Vector { reg: out, .. } = *data else {
            return Err("a BVH intersection result");
        };
        let guard = self.guard(b, i)?;
        let c = self.cur;
        let i64t = self.s.m.int(64);
        // The node's byte offset from the BVH's base: (pointer & !7) * 8.
        let not7 = self.s.m.i32(!7);
        let lo = self.s.m.bin(c, Bin::And, node, not7);
        let nodes = match self.descriptors.get(&((b, i), false)).copied() {
            Some(src) if matches!(src, Source::Built { .. }) => {
                // shortcut: the root address is taken to be 256-byte aligned
                // (the descriptor drops its low byte, not known here).
                let (k, dynamic) = self.built_offset(src)?;
                let three = self.s.m.i32(3);
                let mut at = self.s.m.bin(c, Bin::Shl, lo, three);
                let k = self.s.m.i32(k);
                at = self.s.m.bin(c, Bin::Add, at, k);
                if let Some(d) = dynamic {
                    at = self.s.m.bin(c, Bin::Add, at, d);
                }
                // A root view is title memory there (bounds-checked).
                match self.root_address(src) {
                    Some((root_lo, root_hi)) => {
                        self.s.flags |= super::flags::INT64;
                        let base = wide64(self, root_lo, root_hi);
                        let zero = self.s.m.i32(0);
                        let at = wide64(self, at, zero);
                        Nodes::Raw(self.s.m.bin(c, Bin::Add, base, at))
                    }
                    None => Nodes::Buffer(self.handle((b, i), src, false, (11, 0))?, at),
                }
            }
            Some(_) => return Err("a BVH descriptor"),
            None => {
                // base = (dword 0 | dword 1 bits 0-7 << 32) << 8.
                let Operand::Scalar { code, .. } = *rsrc else {
                    return Err("a BVH descriptor");
                };
                let mut d = Vec::new();
                for k in 0..2 {
                    let v = inst.read(Reg::S(code + k)).ok_or("a BVH descriptor")?;
                    d.push(self.i32_of(v)?);
                }
                self.s.flags |= super::flags::INT64;
                let ff = self.s.m.i32(0xFF);
                let hi = self.s.m.bin(c, Bin::And, d[1], ff);
                let base = wide64(self, d[0], hi);
                let k8 = self.s.m.int_const(i64t, 8);
                let base = self.s.m.bin(c, Bin::Shl, base, k8);
                let zero = self.s.m.i32(0);
                let ptr = wide64(self, lo, node_hi.unwrap_or(zero));
                let k3 = self.s.m.int_const(i64t, 3);
                let off = self.s.m.bin(c, Bin::Shl, ptr, k3);
                Nodes::Raw(self.s.m.bin(c, Bin::Add, base, off))
            }
        };
        // Inactive lanes may hold any pointer: they read nothing.
        let at = self.begin_guarded(guard);
        let hw_op = super::host_hw_bvh();
        let r = match nodes {
            Nodes::Raw(address) if hw_op != 0 => self.bvh_hw(address, node, &ray, hw_op),
            _ => self.bvh_lanes(&nodes, node, &ray),
        };
        let r = match r {
            Ok(r) => r,
            Err(why) => {
                self.end_guarded(at, &[]);
                return Err(why);
            }
        };
        let zero = self.s.m.i32(0);
        let r = self.end_guarded(at, &r.map(|x| (x, zero)));
        for (k, &x) in r.iter().enumerate() {
            self.write(inst, Reg::V(out + k as u8), x, guard);
        }
        Ok(())
    }

    /// Hardware BVH ray-node intersection via AMD D3D12 shader intrinsics
    /// (`AmdExtD3DShaderIntrinsics_IntersectBvhNode`, opcode `0x22`).
    ///
    /// Translates the guest 64-bit node address to a 64-bit host GPU VA via
    /// the page VA table (`t`[`crate::emit::MEMORY_VAS`] in `MEMORY_SPACE`),
    /// and falls back to [`Self::bvh_lanes`] for unmapped pages (recording
    /// misses in the dirty bitmap), partial or unaligned copies, and 128-byte
    /// box32 nodes straddling a 64 KiB page boundary.
    fn bvh_hw(
        &mut self,
        address: V,
        node: V,
        ray: &Ray,
        opcode: u8,
    ) -> Result<[V; 4], Why> {
        let head = self.cur;
        let i32t = self.s.t.i32;
        let i64t = self.s.m.int(64);
        let k32 = self.s.m.int_const(i64t, 32);
        let lo = self.s.m.cast(head, Cast::Trunc, address, i32t);
        let hi_shifted = self.s.m.bin(head, Bin::LShr, address, k32);
        let hi_raw = self.s.m.cast(head, Cast::Trunc, hi_shifted, i32t);
        let mask16 = self.s.m.i32(0xFFFF);
        let hi = self.s.m.bin(head, Bin::And, hi_raw, mask16);

        let (k5, k6, k7) = (self.s.m.i32(5), self.s.m.i32(6), self.s.m.i32(7));
        let kind = self.s.m.bin(head, Bin::And, node, k7);
        let is_valid = self.s.m.cmp(head, pred::IULT, kind, k6);
        let lookup_block = self.s.m.new_block();
        let hw_block = self.s.m.new_block();
        let sw_block = self.s.m.new_block();
        let join = self.s.m.new_block();
        self.s
            .m
            .terminate(head, Term::CondBr(is_valid, lookup_block, join));

        // Look up the 64-bit host GPU VA for this 64 KiB guest page.
        self.cur = lookup_block;
        let (entry, _block, _l1) = self.memory_page(lo, hi);
        let lc = self.cur;
        let vas = self.s.memory_vas_handle();
        let (k3, iu, both, align) = (
            self.s.m.i32(3),
            self.s.m.undef(i32t),
            self.s.m.i8(3),
            self.s.m.i32(4),
        );
        let va_off = self.s.m.bin(lc, Bin::Shl, entry, k3);
        let loaded = self.s.op(
            lc,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            self.s.t.res_ret,
            139,
            &[vas, va_off, iu, both, align],
        );
        let f_lo = self.s.m.extract(lc, loaded, 0);
        let va_lo = self.as_int(f_lo);
        let f_hi = self.s.m.extract(lc, loaded, 1);
        let va_hi = self.as_int(f_hi);

        let zero = self.s.m.i32(0);
        let within = self.s.m.bin(lc, Bin::And, lo, mask16);
        let va_or = self.s.m.bin(lc, Bin::Or, va_lo, va_hi);
        let mapped = self.s.m.cmp(lc, pred::NE, va_or, zero);
        let mask63 = self.s.m.i32(63);
        let va_or_within = self.s.m.bin(lc, Bin::Or, va_lo, within);
        let low6 = self.s.m.bin(lc, Bin::And, va_or_within, mask63);
        let aligned = self.s.m.cmp(lc, pred::EQ, low6, zero);
        let is_box32 = self.s.m.cmp(lc, pred::EQ, kind, k5);
        let max_box32 = self.s.m.i32(0xFF80);
        let max_node64 = self.s.m.i32(0xFFC0);
        let max_within = self.s.m.select(lc, is_box32, max_box32, max_node64);
        let fits_in_page = self.s.m.cmp(lc, pred::IULE, within, max_within);
        let ok0 = self.s.m.bin(lc, Bin::And, mapped, aligned);
        let use_hw = self.s.m.bin(lc, Bin::And, ok0, fits_in_page);
        self.s
            .m
            .terminate(lc, Term::CondBr(use_hw, hw_block, sw_block));

        // Hardware BVH4 intersection via AMD's shader-intrinsics magic UAV.
        self.cur = hw_block;
        let node_lo = self.s.m.bin(hw_block, Bin::Add, va_lo, within);
        let carry_bit = self.s.m.cmp(hw_block, pred::IULT, node_lo, va_lo);
        let carry = self.s.m.cast(hw_block, Cast::ZExt, carry_bit, i32t);
        let node_hi = self.s.m.bin(hw_block, Bin::Add, va_hi, carry);
        let k29 = self.s.m.i32(29);
        let sh_lo = self.s.m.bin(hw_block, Bin::LShr, node_lo, k3);
        let hi_bits = self.s.m.bin(hw_block, Bin::Shl, node_hi, k29);
        let ptr_lo_raw = self.s.m.bin(hw_block, Bin::Or, sh_lo, hi_bits);
        let ptr_lo = self.s.m.bin(hw_block, Bin::Or, ptr_lo_raw, kind);
        let ptr_hi = self.s.m.bin(hw_block, Bin::LShr, node_hi, k3);

        let amd_uav = self.s.amd_ext_handle();
        let phase0 = self.s.m.i32(0x5000_0000 | u32::from(opcode));
        let phase1 = self.s.m.i32(0x5100_0000 | u32::from(opcode));
        let f_extent = self.as_int(ray.extent);
        let f_orig = [
            self.as_int(ray.origin[0]),
            self.as_int(ray.origin[1]),
            self.as_int(ray.origin[2]),
        ];
        let f_dir = [
            self.as_int(ray.dir[0]),
            self.as_int(ray.dir[1]),
            self.as_int(ray.dir[2]),
        ];
        let f_inv = [
            self.as_int(ray.inv[0]),
            self.as_int(ray.inv[1]),
            self.as_int(ray.inv[2]),
        ];
        // AmdExtIntersectBvhNodeFlag: BoxSortEnable (1) | ReturnBarycentrics (2),
        // zero box growth (`0`).
        let flags_val = self.s.m.i32(3);
        let args: [V; 14] = [
            ptr_lo, ptr_hi, f_extent, f_orig[0], f_orig[1], f_orig[2], f_dir[0], f_dir[1],
            f_dir[2], f_inv[0], f_inv[1], f_inv[2], flags_val, zero,
        ];
        for (idx, &val) in args.iter().enumerate() {
            let idx_c = self.s.m.i32(idx as u32);
            self.s.op(
                hw_block,
                "atomicCompareExchange.i32",
                Attr::NoUnwind,
                i32t,
                79,
                &[amd_uav, phase0, iu, iu, idx_c, val],
            );
        }
        let mut hw_res = [zero; 4];
        for (idx, dst) in hw_res.iter_mut().enumerate() {
            let idx_c = self.s.m.i32(idx as u32);
            *dst = self.s.op(
                hw_block,
                "atomicCompareExchange.i32",
                Attr::NoUnwind,
                i32t,
                79,
                &[amd_uav, phase1, iu, iu, idx_c, zero],
            );
        }
        self.s.m.terminate(hw_block, Term::Br(join));

        // Software fallback for unmapped/partial/unaligned/page-straddling nodes.
        self.cur = sw_block;
        let sw_res = self.bvh_lanes(&Nodes::Raw(address), node, ray)?;
        let sw_end = self.cur;
        self.s.m.terminate(sw_end, Term::Br(join));

        self.cur = join;
        let none = self.s.m.i32(INVALID);
        let mut out = [zero; 4];
        for k in 0..4 {
            let phi = self.s.m.phi(join, i32t);
            self.s
                .m
                .set_phi(phi, vec![(none, head), (hw_res[k], hw_block), (sw_res[k], sw_end)]);
            out[k] = phi;
        }
        Ok(out)
    }

    /// The four result dwords of `ray` against the node `node` points to.
    fn bvh_lanes(&mut self, nodes: &Nodes, node: V, ray: &Ray) -> Result<[V; 4], Why> {
        let w = self.node_dwords(nodes, 0, 16)?;
        let c = self.cur;
        let seven = self.s.m.i32(7);
        let ty = self.s.m.bin(c, Bin::And, node, seven);
        // The rest of a box32 node, read only for one.
        let five = self.s.m.i32(5);
        let is32 = self.s.m.cmp(c, pred::EQ, ty, five);
        let at = self.begin_guarded(Some(is32));
        let tail = match self.node_dwords(nodes, 16, 12) {
            Ok(t) => t,
            Err(why) => {
                self.end_guarded(at, &[]);
                return Err(why);
            }
        };
        let zero = self.s.m.i32(0);
        let pairs: Vec<(V, V)> = tail.iter().map(|&t| (t, zero)).collect();
        let tail = self.end_guarded(at, &pairs);
        let w: Vec<V> = w.into_iter().chain(tail).collect();
        let boxes = self.bvh_box(&w, ty, ray);
        let tri = self.bvh_triangle(&w, ty, ray);
        let c = self.cur;
        let four = self.s.m.i32(4);
        let six = self.s.m.i32(6);
        let is_tri = self.s.m.cmp(c, pred::IULT, ty, four);
        let t6 = self.s.m.bin(c, Bin::And, ty, six);
        let is_box = self.s.m.cmp(c, pred::EQ, t6, four);
        let none = self.s.m.i32(INVALID);
        Ok(std::array::from_fn(|k| {
            let x = self.s.m.select(c, is_box, boxes[k], none);
            self.s.m.select(c, is_tri, tri[k], x)
        }))
    }

    /// `n` dwords of the node from dword `from`.
    fn node_dwords(&mut self, nodes: &Nodes, from: u32, n: u32) -> Result<Vec<V>, Why> {
        let (h, at) = match *nodes {
            Nodes::Buffer(h, at) => (h, at),
            Nodes::Raw(address) => return self.raw_load(address, 4 * from, n),
        };
        let c = self.cur;
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let mut out = Vec::new();
        for k in (0..n).step_by(4) {
            let m = (n - k).min(4);
            let off = self.s.m.i32(4 * (from + k));
            let off = self.s.m.bin(c, Bin::Add, at, off);
            let mask = self.s.m.i8((1u8 << m) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, off, u, mask, align],
            );
            for j in 0..m {
                let x = self.s.m.extract(c, r, j);
                out.push(self.as_int(x));
            }
        }
        Ok(out)
    }

    /// `dwords` dwords at byte `offset` past `address`, a 64-bit GPU address
    /// (`i64`), from title memory ([`Lower::memory`]).
    fn raw_load(&mut self, address: V, offset: u32, dwords: u32) -> Result<Vec<V>, Why> {
        let c = self.cur;
        let i64t = self.s.m.int(64);
        let off = self.s.m.int_const(i64t, i64::from(offset));
        let a = self.s.m.bin(c, Bin::Add, address, off);
        let k32 = self.s.m.int_const(i64t, 32);
        let lo = self.s.m.cast(c, Cast::Trunc, a, self.s.t.i32);
        let hi = self.s.m.bin(c, Bin::LShr, a, k32);
        let hi = self.s.m.cast(c, Cast::Trunc, hi, self.s.t.i32);
        // Address bits 32-47, as the hardware takes them.
        let mask = self.s.m.i32(0xFFFF);
        let hi = self.s.m.bin(c, Bin::And, hi, mask);
        // A node (64 bytes, 64-byte aligned) lies in one 64 KiB page.
        let (h, at) = self.memory(lo, hi, 4 * dwords, false, None);
        let c = self.cur;
        let (u, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        let mut out = Vec::new();
        for k in (0..dwords).step_by(4) {
            let m = (dwords - k).min(4);
            let o = self.s.m.i32(4 * k);
            let o = self.s.m.bin(c, Bin::Add, at, o);
            let mask = self.s.m.i8((1u8 << m) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, o, u, mask, align],
            );
            for j in 0..m {
                let x = self.s.m.extract(c, r, j);
                out.push(self.as_int(x));
            }
        }
        Ok(out)
    }

    /// The child pointers of a box node (`w`: its 28 dwords, past 16 zero
    /// unless box32), sorted by distance.
    fn bvh_box(&mut self, w: &[V], ty: V, ray: &Ray) -> [V; 4] {
        let c = self.cur;
        let four = self.s.m.i32(4);
        let is16 = self.s.m.cmp(c, pred::EQ, ty, four);
        let f32t = self.s.t.f32;
        let zero = self.s.m.f32(0);
        let (inf, ninf) = (
            self.s.m.f32(f32::INFINITY.to_bits()),
            self.s.m.f32(f32::NEG_INFINITY.to_bits()),
        );
        let none = self.s.m.i32(INVALID);
        let mut child = [none; 4];
        let mut key = [zero; 4];
        for i in 0..4 {
            // min xyz, max xyz.
            let mut b = [zero; 6];
            for (j, x) in b.iter_mut().enumerate() {
                let wide = self.as_float(w[4 + 6 * i + j]);
                // f16 pairs: minx|miny, minz|maxx, maxy|maxz.
                let half = w[4 + 3 * i + j / 2];
                let half = if j % 2 == 1 {
                    let k16 = self.s.m.i32(16);
                    self.s.m.bin(c, Bin::LShr, half, k16)
                } else {
                    half
                };
                let half = self
                    .s
                    .op(c, "legacyF16ToF32", Attr::ReadNone, f32t, 131, &[half]);
                *x = self.s.m.select(c, is16, half, wide);
            }
            let mut nan = self.s.m.bool(false);
            let (mut near, mut far) = (zero, inf);
            for a in 0..3 {
                let dlo = self.s.m.bin(c, Bin::Sub, b[a], ray.origin[a]);
                let dhi = self.s.m.bin(c, Bin::Sub, b[3 + a], ray.origin[a]);
                let tlo = self.s.m.bin(c, Bin::Mul, dlo, ray.inv[a]);
                let thi = self.s.m.bin(c, Bin::Mul, dhi, ray.inv[a]);
                let pos = self.s.m.cmp(c, pred::OGE, ray.inv[a], zero);
                let mut n = self.s.m.select(c, pos, tlo, thi);
                let mut x = self.s.m.select(c, pos, thi, tlo);
                // A ray in the plane (0 * inf) is not bounded by it.
                for (t, d, open) in [(&mut n, [dlo, dhi], ninf), (&mut x, [dhi, dlo], inf)] {
                    let d = self.s.m.select(c, pos, d[0], d[1]);
                    let on = self.s.m.cmp(c, pred::OEQ, d, zero);
                    let undefined = self.s.m.cmp(c, pred::UNO, *t, *t);
                    let both = self.s.m.bin(c, Bin::And, on, undefined);
                    *t = self.s.m.select(c, both, open, *t);
                }
                for t in [n, x] {
                    let undefined = self.s.m.cmp(c, pred::UNO, t, t);
                    nan = self.s.m.bin(c, Bin::Or, nan, undefined);
                }
                let more = self.s.m.cmp(c, pred::OGT, n, near);
                near = self.s.m.select(c, more, n, near);
                let less = self.s.m.cmp(c, pred::OLT, x, far);
                far = self.s.m.select(c, less, x, far);
            }
            // A NaN extent bounds nothing.
            let less = self.s.m.cmp(c, pred::OLT, ray.extent, far);
            far = self.s.m.select(c, less, ray.extent, far);
            let within = self.s.m.cmp(c, pred::OLE, near, far);
            let t = self.s.m.bool(true);
            let not_nan = self.s.m.bin(c, Bin::Xor, nan, t);
            let hit = self.s.m.bin(c, Bin::And, within, not_nan);
            child[i] = self.s.m.select(c, hit, w[i], none);
            key[i] = near;
        }
        // GPURT's network: a valid child moves before a farther or invalid one.
        for (a, b) in [(0, 2), (1, 3), (0, 1), (2, 3), (1, 2)] {
            let valid_b = self.s.m.cmp(c, pred::NE, child[b], none);
            let closer = self.s.m.cmp(c, pred::OLT, key[b], key[a]);
            let move_b = self.s.m.bin(c, Bin::And, valid_b, closer);
            let invalid_a = self.s.m.cmp(c, pred::EQ, child[a], none);
            let swap = self.s.m.bin(c, Bin::Or, move_b, invalid_a);
            let (ca, cb) = (child[a], child[b]);
            child[a] = self.s.m.select(c, swap, cb, ca);
            child[b] = self.s.m.select(c, swap, ca, cb);
            let (ka, kb) = (key[a], key[b]);
            key[a] = self.s.m.select(c, swap, kb, ka);
            key[b] = self.s.m.select(c, swap, ka, kb);
        }
        child
    }

    /// The triangle result of a triangle node (`w`: its 16 dwords).
    fn bvh_triangle(&mut self, w: &[V], ty: V, ray: &Ray) -> [V; 4] {
        let c = self.cur;
        let zero = self.s.m.f32(0);
        // The vertices type 0, 1, 2 or 3 tests.
        let is = |l: &mut Self, t: u32| {
            let k = l.s.m.i32(t);
            l.s.m.cmp(c, pred::EQ, ty, k)
        };
        let (t1, t2, t3) = (is(self, 1), is(self, 2), is(self, 3));
        let vertex = |l: &mut Self, v: usize| -> [V; 3] {
            std::array::from_fn(|k| l.as_float(w[3 * v + k]))
        };
        let pick = |l: &mut Self, of: [usize; 4]| -> [V; 3] {
            let vs = of.map(|v| vertex(l, v));
            std::array::from_fn(|k| {
                let x = l.s.m.select(c, t1, vs[1][k], vs[0][k]);
                let x = l.s.m.select(c, t2, vs[2][k], x);
                l.s.m.select(c, t3, vs[3][k], x)
            })
        };
        let p = [
            pick(self, [0, 1, 2, 2]),
            pick(self, [1, 3, 3, 4]),
            pick(self, [2, 2, 4, 0]),
        ];
        // kz: the axis of the largest |direction| (ties to the lower axis),
        // kx and ky the next ones, swapped where the direction along kz is
        // negative (which keeps the winding).
        let abs = |l: &mut Self, x: V| {
            let i = l.as_int(x);
            let m = l.s.m.i32(0x7FFF_FFFF);
            let i = l.s.m.bin(c, Bin::And, i, m);
            l.as_float(i)
        };
        let ad = ray.dir.map(|d| abs(self, d));
        let x_ge_y = self.s.m.cmp(c, pred::OGE, ad[0], ad[1]);
        let x_ge_z = self.s.m.cmp(c, pred::OGE, ad[0], ad[2]);
        let y_ge_z = self.s.m.cmp(c, pred::OGE, ad[1], ad[2]);
        let z0 = self.s.m.bin(c, Bin::And, x_ge_y, x_ge_z);
        let t = self.s.m.bool(true);
        let y_gt_x = self.s.m.bin(c, Bin::Xor, x_ge_y, t);
        let z1 = self.s.m.bin(c, Bin::And, y_gt_x, y_ge_z);
        let by = |l: &mut Self, v: [V; 3], k: [usize; 3]| {
            let x = l.s.m.select(c, z1, v[k[1]], v[k[2]]);
            l.s.m.select(c, z0, v[k[0]], x)
        };
        let dz = by(self, ray.dir, [0, 1, 2]);
        let neg = self.s.m.cmp(c, pred::OLT, dz, zero);
        // (x, y, z) components in the sheared frame's axis order.
        let perm = |l: &mut Self, v: [V; 3]| -> [V; 3] {
            let x = by(l, v, [1, 2, 0]);
            let y = by(l, v, [2, 0, 1]);
            let z = by(l, v, [0, 1, 2]);
            [l.s.m.select(c, neg, y, x), l.s.m.select(c, neg, x, y), z]
        };
        let d = perm(self, ray.dir);
        let one = self.s.m.f32(1.0f32.to_bits());
        let sz = self.s.m.bin(c, Bin::SDiv, one, d[2]);
        let sx = self.s.m.bin(c, Bin::Mul, d[0], sz);
        let sy = self.s.m.bin(c, Bin::Mul, d[1], sz);
        let sheared = |l: &mut Self, v: [V; 3]| -> [V; 3] {
            let rel: [V; 3] = std::array::from_fn(|k| l.s.m.bin(c, Bin::Sub, v[k], ray.origin[k]));
            let r = perm(l, rel);
            let tx = l.s.m.bin(c, Bin::Mul, sx, r[2]);
            let ty = l.s.m.bin(c, Bin::Mul, sy, r[2]);
            [
                l.s.m.bin(c, Bin::Sub, r[0], tx),
                l.s.m.bin(c, Bin::Sub, r[1], ty),
                l.s.m.bin(c, Bin::Mul, sz, r[2]),
            ]
        };
        let [a, b, cc] = p.map(|v| sheared(self, v));
        let edge = |l: &mut Self, p: [V; 3], q: [V; 3]| {
            let m = l.s.m.bin(c, Bin::Mul, p[0], q[1]);
            let n = l.s.m.bin(c, Bin::Mul, p[1], q[0]);
            l.s.m.bin(c, Bin::Sub, m, n)
        };
        let u = edge(self, cc, b);
        let v = edge(self, a, cc);
        let ww = edge(self, b, a);
        let all = |l: &mut Self, p: u8| {
            let x = l.s.m.cmp(c, p, u, zero);
            let y = l.s.m.cmp(c, p, v, zero);
            let z = l.s.m.cmp(c, p, ww, zero);
            let xy = l.s.m.bin(c, Bin::And, x, y);
            l.s.m.bin(c, Bin::And, xy, z)
        };
        let (ge, le) = (all(self, pred::OGE), all(self, pred::OLE));
        let inside = self.s.m.bin(c, Bin::Or, ge, le);
        let vw = self.s.m.bin(c, Bin::Add, v, ww);
        let det = self.s.m.bin(c, Bin::Add, u, vw);
        let ua = self.s.m.bin(c, Bin::Mul, u, a[2]);
        let vb = self.s.m.bin(c, Bin::Mul, v, b[2]);
        let wc = self.s.m.bin(c, Bin::Mul, ww, cc[2]);
        let uv = self.s.m.bin(c, Bin::Add, ua, vb);
        let tnum = self.s.m.bin(c, Bin::Add, uv, wc);
        // t >= 0: the numerator has the denominator's sign (0 misses).
        let side = |l: &mut Self, p: u8, q: u8| {
            let x = l.s.m.cmp(c, p, det, zero);
            let y = l.s.m.cmp(c, q, tnum, zero);
            l.s.m.bin(c, Bin::And, x, y)
        };
        let (front, back) = (
            side(self, pred::OGT, pred::OGE),
            side(self, pred::OLT, pred::OLE),
        );
        let ahead = self.s.m.bin(c, Bin::Or, front, back);
        let hit = self.s.m.bin(c, Bin::And, inside, ahead);
        // Barycentrics as the triangle id's bits for this type pick them.
        let three = self.s.m.i32(3);
        let t3b = self.s.m.bin(c, Bin::And, ty, three);
        let sh = self.s.m.bin(c, Bin::Shl, t3b, three);
        let id = self.s.m.bin(c, Bin::LShr, w[15], sh);
        let bary = [u, v, ww].map(|x| self.as_int(x));
        let nil = self.s.m.i32(0);
        let pickb = |l: &mut Self, at: u32| {
            let k = l.s.m.i32(at);
            let s = l.s.m.bin(c, Bin::LShr, id, k);
            let s = l.s.m.bin(c, Bin::And, s, three);
            let mut x = nil;
            for (j, &bj) in bary.iter().enumerate() {
                let jk = l.s.m.i32(j as u32);
                let is = l.s.m.cmp(c, pred::EQ, s, jk);
                x = l.s.m.select(c, is, bj, x);
            }
            l.s.m.select(c, hit, x, nil)
        };
        let (bi, bj) = (pickb(self, 0), pickb(self, 2));
        let inf = self.s.m.i32(f32::INFINITY.to_bits());
        let onei = self.s.m.i32(1.0f32.to_bits());
        let tn = self.as_int(tnum);
        let dn = self.as_int(det);
        [
            self.s.m.select(c, hit, tn, inf),
            self.s.m.select(c, hit, dn, onei),
            bi,
            bj,
        ]
    }
}

/// `lo | hi << 32` as an `i64`.
fn wide64(l: &mut Lower<'_>, lo: V, hi: V) -> V {
    let c = l.cur;
    let i64t = l.s.m.int(64);
    let lo = l.s.m.cast(c, Cast::ZExt, lo, i64t);
    let hi = l.s.m.cast(c, Cast::ZExt, hi, i64t);
    let k32 = l.s.m.int_const(i64t, 32);
    let hi = l.s.m.bin(c, Bin::Shl, hi, k32);
    l.s.m.bin(c, Bin::Or, lo, hi)
}
