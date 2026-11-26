//! A DXIL vertex, pixel, compute or mesh shader under construction: the module,
//! the `dx.op` declarations it calls, its signatures and bound resources,
//! and the metadata and container that wrap it up (SM 6.6, validator 1.6).

use super::bitcode::{Attr, Md, Module, Op, Term, Ty, V, pred};
use super::container::{self, Binding, Psv, SigElement};

/// Where the shader reads its user data: `b0` in this space.
pub const USER_DATA_SPACE: u32 = 0;
/// Where immediate samplers are bound: `s<n>` in this space.
pub const SAMPLER_SPACE: u32 = 0;
/// Where root CBVs and SRVs are bound: `b<root parameter index>` and
/// `t<root parameter index>` in this space.
pub const ROOT_CBV_SPACE: u32 = 1;
/// The root SRV slot (`t255` in [`ROOT_CBV_SPACE`], past every title root
/// parameter) of the view-stride table: dword `i` is the byte stride of
/// the buffer view at view-heap index `i` ([`VIEW_STRIDE_BITS`]), with
/// [`VIEW_INTEGER`] and [`VIEW_SIGNED`] for typed views of integer formats.
pub const VIEW_STRIDES: u8 = 255;
/// The bits of a view-stride table dword holding the stride.
pub const VIEW_STRIDE_BITS: u32 = (1 << 30) - 1;
/// In a view-stride table dword: a typed buffer view of an integer format
/// (`UINT` or `SINT`), whose 16-bit typed accesses (`_d16`) pass bits.
pub const VIEW_INTEGER: u32 = 1 << 31;
/// With [`VIEW_INTEGER`]: a signed (`SINT`) format.
pub const VIEW_SIGNED: u32 = 1 << 30;
/// Where the title-memory page table is bound: `t0` (a raw root SRV) in
/// this space (see `emit` "Title memory").
pub const MEMORY_SPACE: u32 = 2;
/// Heap indices are clamped below these (views, samplers): the host's heaps
/// are at least this large, the slots past their own null (`emit`, "Heap
/// indices").
pub const HEAP_VIEWS: u32 = 1 << 21;
pub const HEAP_SAMPLERS: u32 = 2048;
/// The heap index of the host's null raw buffer view (the null raw
/// unordered access view at the index below), read instead of a slot that
/// holds no buffer view where a shader reads one at an index it computes
/// (`emit`, "Buffer views at computed heap indices").
pub const NULL_BUFFER_VIEW: u32 = HEAP_VIEWS - 2;
/// The view-mark table's register: `t2` (a raw root SRV) in
/// [`MEMORY_SPACE`] (`emit`, "Write marks").
pub const VIEW_MARKS: u32 = 2;
/// The written-pages bitmap of buffer views: `u1` (a raw root UAV) in
/// [`MEMORY_SPACE`].
pub const VIEW_WRITES: u32 = 1;
/// A view-mark table entry's base for a slot whose writes are not marked.
pub const VIEW_MARK_NONE: u32 = u32::MAX;
/// Bits in the written-pages bitmap of buffer views; marks past it go to
/// the last bit.
pub const VIEW_WRITE_BITS: u32 = 1 << 22;
/// The first bit of the read marks of texture views (`emit`, "Read
/// marks"), after the written pages in the same bitmap: one bit per
/// resource-heap index, [`HEAP_VIEWS`] of them.
pub const VIEW_READS: u32 = VIEW_WRITE_BITS;

/// `ShaderFlags` bits (metadata) and their `SFI0` feature bits.
pub(crate) mod flags {
    pub const RAW_BUFFERS: u64 = 1 << 4;
    pub const STENCIL_REF: u64 = 1 << 11;
    pub const TYPED_UAV_LOADS: u64 = 1 << 13;
    /// UAVs in a vertex, geometry or mesh program.
    pub const UAVS_EVERY_STAGE: u64 = 1 << 16;
    pub const ROVS: u64 = 1 << 18;
    pub const WAVE_OPS: u64 = 1 << 19;
    pub const INT64: u64 = 1 << 20;
    pub const ATOMIC64_TYPED: u64 = 1 << 27;
    pub const RESOURCE_HEAP: u64 = 1 << 30;
    pub const SAMPLER_HEAP: u64 = 1 << 31;
    pub const ATOMIC64_HEAP: u64 = 1 << 32;
}

pub(crate) struct Types {
    pub void: Ty,
    pub i1: Ty,
    pub i32: Ty,
    pub f32: Ty,
    pub handle: Ty,
    pub props: Ty,
    pub bind: Ty,
    pub cbuf_ret: Ty,
    pub res_ret: Ty,
}

/// What a mesh shader declares beyond its thread-group size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mesh {
    pub max_vertices: u32,
    pub max_primitives: u32,
    /// 1 lines, 2 triangles.
    pub topology: u8,
}

pub(crate) struct Shader {
    pub m: Module,
    pub t: Types,
    pub pixel: bool,
    pub inputs: Vec<SigElement>,
    pub outputs: Vec<SigElement>,
    /// A mesh shader's per-primitive outputs.
    pub prims: Vec<SigElement>,
    /// A mesh shader (`threads` is its thread-group size).
    pub mesh: Option<Mesh>,
    /// A geometry shader taking triangles and emitting triangle strips of
    /// at most this many vertices (`rect`).
    pub gs: Option<u32>,
    /// Dwords of user data the shader reads (0: no user-data buffer).
    pub user_data: u32,
    /// Immediate samplers, bound as `s0..` in [`SAMPLER_SPACE`].
    pub samplers: u32,
    /// Bit n: immediate sampler n is a comparison sampler.
    pub cmp_samplers: u64,
    /// Immediate samplers uniform control flow chooses between: handles
    /// `a` and `b` and when `a` is taken. An operation on `a` runs on both,
    /// and the result is chosen.
    pub sampler_choices: Vec<(V, V, V)>,
    /// Root CBVs read, by root parameter index, with their handles.
    pub root_cbvs: Vec<(u8, V)>,
    /// Root SRVs (raw buffers) read, likewise.
    pub root_srvs: Vec<(u8, V)>,
    /// Root UAVs (raw buffers) read or written, likewise.
    pub root_uavs: Vec<(u8, V)>,
    /// The title-memory page table's handle, once read.
    pub memory: Option<V>,
    /// The written-pages bitmap's handle, once title memory is written.
    pub memory_dirty: Option<V>,
    /// The view addresses' handle, once a descriptor's address is read.
    pub view_addresses: Option<V>,
    /// The view-mark table's and the buffer views' written-pages bitmap's
    /// handles, once a write through a view is marked.
    pub view_marks: Option<V>,
    pub view_writes: Option<V>,
    /// The heap index of each handle of an unordered buffer view (writes
    /// through it are marked by the index's view-mark entry).
    pub view_index: std::collections::HashMap<V, V>,
    /// The heap index of each handle of a texture shader view (its reads
    /// are marked, `emit` "Read marks"), and those the instruction being
    /// lowered uses.
    pub texture_index: std::collections::HashMap<V, V>,
    pub texture_reads: Vec<V>,
    pub flags: u64,
    /// Heap handles made with a non-uniform index.
    pub nonuniform_handles: u32,
    /// A compute shader with this thread-group size.
    pub threads: Option<[u32; 3]>,
    /// The wave size a compute shader requires (`WaveSize`).
    pub wave_size: Option<u32>,
    /// Thread-group shared memory, in dwords.
    pub lds_dwords: u32,
    lds: Option<V>,
    main: V,
    pub entry: usize,
}

impl Shader {
    pub fn new(pixel: bool) -> Self {
        let mut m = Module::default();
        let void = m.void();
        let i1 = m.int(1);
        let i8 = m.int(8);
        let i32 = m.int(32);
        let f32 = m.float();
        let i8p = m.ptr(i8);
        let handle = m.named_struct("dx.types.Handle", vec![i8p]);
        let props = m.named_struct("dx.types.ResourceProperties", vec![i32, i32]);
        let bind = m.named_struct("dx.types.ResBind", vec![i32, i32, i32, i8]);
        let cbuf_ret = m.named_struct("dx.types.CBufRet.i32", vec![i32, i32, i32, i32]);
        let res_ret = m.named_struct("dx.types.ResRet.f32", vec![f32, f32, f32, f32, i32]);
        let main_ty = m.func_ty(void, vec![]);
        m.ptr(main_ty);
        let main = m.function("main", main_ty, None, true);
        let entry = m.new_block();
        Shader {
            m,
            t: Types {
                void,
                i1,
                i32,
                f32,
                handle,
                props,
                bind,
                cbuf_ret,
                res_ret,
            },
            pixel,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh: None,
            gs: None,
            user_data: 0,
            samplers: 0,
            cmp_samplers: 0,
            sampler_choices: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            memory: None,
            memory_dirty: None,
            view_addresses: None,
            view_marks: None,
            view_writes: None,
            view_index: std::collections::HashMap::new(),
            texture_index: std::collections::HashMap::new(),
            texture_reads: Vec::new(),
            flags: 0,
            nonuniform_handles: 0,
            threads: None,
            wave_size: None,
            lds_dwords: 0,
            lds: None,
            main,
            entry,
        }
    }

    /// The thread-group shared memory: `[lds_dwords x i32]` in address
    /// space 3.
    pub fn lds(&mut self) -> V {
        if let Some(g) = self.lds {
            return g;
        }
        let arr = self.m.array(self.lds_dwords, self.t.i32);
        let g = self.m.global("lds", arr, 3);
        self.lds = Some(g);
        g
    }

    /// Calls `dx.op.<name>` (class and overload) with `opcode` and `args`.
    pub fn op(
        &mut self,
        block: usize,
        name: &str,
        attr: Attr,
        ret: Ty,
        opcode: u32,
        args: &[V],
    ) -> V {
        let mut params = vec![self.t.i32];
        params.extend(args.iter().map(|&a| self.m.type_of(a)));
        let fty = self.m.func_ty(ret, params);
        self.m.ptr(fty);
        let f = self
            .m
            .function(&format!("dx.op.{name}"), fty, Some(attr), false);
        let mut all = vec![self.m.i32(opcode)];
        all.extend(args);
        let choice = self
            .sampler_choices
            .iter()
            .copied()
            .find(|&(a, _, _)| args.contains(&a));
        let r = self.m.call(block, f, all.clone());
        let Some((a, b, cond)) = choice else {
            return r;
        };
        for x in &mut all {
            if *x == a {
                *x = b;
            }
        }
        let other = self.m.call(block, f, all);
        if ret == self.t.res_ret {
            // Drivers miscompile a select of the result structure (AMD
            // read zeros): its fields are chosen where they are read.
            self.m.chosen.insert(r, (cond, other));
            return r;
        }
        self.m.select(block, cond, r, other)
    }

    /// The handle of the user-data constant buffer, made in the entry block.
    pub fn user_data_handle(&mut self) -> V {
        let bind = [
            self.m.i32(0),
            self.m.i32(0),
            self.m.i32(USER_DATA_SPACE),
            self.m.i8(2), // CBV
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let block = self.entry;
        let (i, f) = (self.m.i32(0), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        // The size is known in `finish`, which fixes this placeholder.
        self.annotate(block, h, 13, u32::MAX)
    }

    /// The handle of the root CBV of root parameter `slot`, made in the
    /// entry block.
    pub fn root_cbv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_cbvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(2)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 13, 65536);
        self.root_cbvs.push((slot, h));
        h
    }

    /// The handle of the root SRV (a raw buffer) of root parameter `slot`.
    pub fn root_srv_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_srvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_srvs.push((slot, h));
        h
    }

    /// The handle of the root UAV (a raw buffer) of root parameter `slot`.
    pub fn root_uav_handle(&mut self, slot: u8) -> V {
        if let Some(&(_, h)) = self.root_uavs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let r = self.m.i32(u32::from(slot));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(1)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11 | 0x1000, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.root_uavs.push((slot, h));
        h
    }

    /// The handle of the title-memory page table (`t0` in [`MEMORY_SPACE`],
    /// a raw buffer), made in the entry block.
    pub fn memory_handle(&mut self) -> V {
        if let Some(h) = self.memory {
            return h;
        }
        let r = self.m.i32(0);
        let bind = [r, r, self.m.i32(MEMORY_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.memory = Some(h);
        h
    }

    /// The handle of the view addresses (`t1` in [`MEMORY_SPACE`], a raw
    /// buffer: per resource-heap index, a descriptor's dwords 0-3 as far as
    /// the host knows them, `emit` "View addresses"), made in the entry
    /// block.
    pub fn view_addresses_handle(&mut self) -> V {
        if let Some(h) = self.view_addresses {
            return h;
        }
        let r = self.m.i32(1);
        let bind = [r, r, self.m.i32(MEMORY_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.view_addresses = Some(h);
        h
    }

    /// The handle of the bitmap of title-memory pages written (`u0` in
    /// [`MEMORY_SPACE`], a raw buffer), made in the entry block.
    pub fn memory_dirty_handle(&mut self) -> V {
        if let Some(h) = self.memory_dirty {
            return h;
        }
        let r = self.m.i32(0);
        let bind = [r, r, self.m.i32(MEMORY_SPACE), self.m.i8(1)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, 11 | 0x1000, 0);
        self.flags |= flags::RAW_BUFFERS;
        self.memory_dirty = Some(h);
        h
    }

    /// The handle of the view-mark table (`t2` in [`MEMORY_SPACE`], a raw
    /// buffer: per resource-heap index, two dwords), made in the entry block.
    pub fn view_marks_handle(&mut self) -> V {
        if let Some(h) = self.view_marks {
            return h;
        }
        let h = self.memory_space_handle(VIEW_MARKS, false);
        self.view_marks = Some(h);
        h
    }

    /// The handle of the buffer views' written-pages bitmap (`u1` in
    /// [`MEMORY_SPACE`], a raw buffer), made in the entry block.
    pub fn view_writes_handle(&mut self) -> V {
        if let Some(h) = self.view_writes {
            return h;
        }
        let h = self.memory_space_handle(VIEW_WRITES, true);
        self.view_writes = Some(h);
        h
    }

    /// A raw buffer bound at `register` in [`MEMORY_SPACE`] (`uav`: `u`,
    /// else `t`), made in the entry block.
    fn memory_space_handle(&mut self, register: u32, uav: bool) -> V {
        let r = self.m.i32(register);
        let bind = [r, r, self.m.i32(MEMORY_SPACE), self.m.i8(u8::from(uav))];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, if uav { 11 | 0x1000 } else { 11 }, 0);
        self.flags |= flags::RAW_BUFFERS;
        h
    }

    /// The handle of immediate sampler `n` (a comparison sampler when its
    /// bit in `cmp_samplers` is set).
    pub fn sampler_handle(&mut self, block: usize, n: u32) -> V {
        let bind = [
            self.m.i32(n),
            self.m.i32(n),
            self.m.i32(SAMPLER_SPACE),
            self.m.i8(3), // sampler
        ];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let (i, f) = (self.m.i32(n), self.m.bool(false));
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, i, f],
        );
        let cmp = if self.cmp_samplers >> n & 1 != 0 {
            0x8000
        } else {
            0
        };
        self.annotate(block, h, 14 | cmp, 0)
    }

    pub fn annotate(&mut self, block: usize, h: V, props0: u32, props1: u32) -> V {
        let p = [self.m.i32(props0), self.m.i32(props1)];
        let p = self.m.aggregate(self.t.props, &p);
        self.op(
            block,
            "annotateHandle",
            Attr::ReadNone,
            self.t.handle,
            216,
            &[h, p],
        )
    }

    /// A descriptor heap handle, annotated with `props`; `nonuniform` when
    /// the index may differ between lanes (`NonUniformResourceIndex`).
    pub fn heap_handle(
        &mut self,
        block: usize,
        index: V,
        sampler: bool,
        nonuniform: bool,
        props: (u32, u32),
    ) -> V {
        self.flags |= if sampler {
            flags::SAMPLER_HEAP
        } else {
            flags::RESOURCE_HEAP
        };
        self.nonuniform_handles += u32::from(nonuniform);
        // An index past the host's heap faults the device (a console shader
        // reads whatever memory follows its heap): the last slot instead.
        let last = self.m.i32(if sampler { HEAP_SAMPLERS } else { HEAP_VIEWS } - 1);
        let over = self.m.cmp(block, pred::IUGT, index, last);
        let index = self.m.select(block, over, last, index);
        let (s, n) = (self.m.bool(sampler), self.m.bool(nonuniform));
        let h = self.op(
            block,
            "createHandleFromHeap",
            Attr::ReadNone,
            self.t.handle,
            218,
            &[index, s, n],
        );
        let h = self.annotate(block, h, props.0, props.1);
        // Unordered typed, raw and structured buffers (kinds 10-12), and
        // typed buffers read (their format kind, `Lower::view_word`).
        let kind = props.0 & 0xFF;
        if !sampler && (kind == 10 || props.0 & 0x1000 != 0 && (11..=12).contains(&kind)) {
            self.view_index.insert(h, index);
        }
        // Texture shader views (kinds 1-9).
        if !sampler && props.0 & 0x1000 == 0 && (1..=9).contains(&kind) {
            self.texture_index.insert(h, index);
        }
        h
    }

    /// The finished container.
    pub fn finish(mut self) -> Vec<u8> {
        let ud_size = (self.user_data * 4).next_multiple_of(16);
        self.fix_user_data_size(ud_size);
        // The flags of what lowering made, as the module writes it: what only
        // unread values or never-taken edges used is not written.
        let uses = self.m.uses();
        for (flag, used) in [
            (flags::RESOURCE_HEAP, uses.heap),
            (flags::SAMPLER_HEAP, uses.sampler_heap),
            (flags::WAVE_OPS, uses.wave),
            (flags::INT64, uses.int64),
        ] {
            self.flags = if used { self.flags | flag } else { self.flags & !flag };
        }
        // UAVs outside pixel and compute programs (the write and read marks
        // of vertex, geometry and mesh programs), as DXC counts them: UAV
        // ranges declared below, not UAVs indexed from the heap.
        let compute = self.threads.is_some() && self.mesh.is_none() && self.gs.is_none();
        let declared_uavs =
            !self.root_uavs.is_empty() || self.memory_dirty.is_some() || self.view_writes.is_some();
        if declared_uavs && !self.pixel && !compute {
            self.flags |= flags::UAVS_EVERY_STAGE;
        }
        let mut resources = None;
        let mut bindings = Vec::new();
        let i32c = |s: &mut Self, v: u32| {
            let c = s.m.i32(v);
            s.m.md_value(c)
        };
        if self.user_data > 0
            || self.samplers > 0
            || !self.root_cbvs.is_empty()
            || !self.root_srvs.is_empty()
            || !self.root_uavs.is_empty()
            || self.memory.is_some()
            || self.memory_dirty.is_some()
            || self.view_addresses.is_some()
            || self.view_marks.is_some()
            || self.view_writes.is_some()
        {
            let empty = self.m.md_str("");
            let mut cbvs = None;
            let mut cbv_list = Vec::new();
            if self.user_data > 0 {
                let arr = self.m.array(self.user_data.next_multiple_of(4), self.t.i32);
                let ty = self.m.named_struct("UserData", vec![arr]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let ops = vec![
                    Some(i32c(&mut self, 0)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, USER_DATA_SPACE)),
                    Some(i32c(&mut self, 0)),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, ud_size)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: USER_DATA_SPACE,
                    register: 0,
                    dxil_kind: 13,
                });
            }
            let blob = self.m.array(16384, self.t.i32);
            let ty = self.m.named_struct("RootCbv", vec![blob]);
            let p = self.m.ptr(ty);
            let u = self.m.undef(p);
            for (slot, _) in self.root_cbvs.clone() {
                let ops = vec![
                    Some(i32c(&mut self, cbv_list.len() as u32)),
                    Some(self.m.md_value(u)),
                    Some(empty),
                    Some(i32c(&mut self, ROOT_CBV_SPACE)),
                    Some(i32c(&mut self, u32::from(slot))),
                    Some(i32c(&mut self, 1)),
                    Some(i32c(&mut self, 65536)),
                    None,
                ];
                cbv_list.push(Some(self.m.md_node(ops)));
                bindings.push(Binding {
                    kind: 2,
                    space: ROOT_CBV_SPACE,
                    register: u32::from(slot),
                    dxil_kind: 13,
                });
            }
            if !cbv_list.is_empty() {
                cbvs = Some(self.m.md_node(cbv_list));
            }
            let mut srvs = None;
            if !self.root_srvs.is_empty()
                || self.memory.is_some()
                || self.view_addresses.is_some()
                || self.view_marks.is_some()
            {
                let ty = self
                    .m
                    .named_struct("struct.ByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                let all: Vec<(u32, u32)> = self
                    .root_srvs
                    .iter()
                    .map(|&(slot, _)| (ROOT_CBV_SPACE, u32::from(slot)))
                    .chain(self.memory.map(|_| (MEMORY_SPACE, 0)))
                    .chain(self.view_addresses.map(|_| (MEMORY_SPACE, 1)))
                    .chain(self.view_marks.map(|_| (MEMORY_SPACE, VIEW_MARKS)))
                    .collect();
                for (n, (space, slot)) in all.into_iter().enumerate() {
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, space)),
                        Some(i32c(&mut self, slot)),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, 11)),
                        Some(i32c(&mut self, 0)),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 4,
                        space,
                        register: slot,
                        dxil_kind: 11,
                    });
                }
                srvs = Some(self.m.md_node(list));
            }
            let mut uavs = None;
            if declared_uavs {
                let ty = self
                    .m
                    .named_struct("struct.RWByteAddressBuffer", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let no = self.m.bool(false);
                let no = self.m.md_value(no);
                let mut list = Vec::new();
                let all: Vec<(u32, u32)> = self
                    .root_uavs
                    .iter()
                    .map(|&(slot, _)| (ROOT_CBV_SPACE, u32::from(slot)))
                    .chain(self.memory_dirty.map(|_| (MEMORY_SPACE, 0)))
                    .chain(self.view_writes.map(|_| (MEMORY_SPACE, VIEW_WRITES)))
                    .collect();
                for (n, (space, slot)) in all.into_iter().enumerate() {
                    let ops = vec![
                        Some(i32c(&mut self, n as u32)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, space)),
                        Some(i32c(&mut self, slot)),
                        Some(i32c(&mut self, 1)),
                        Some(i32c(&mut self, 11)),
                        Some(no),
                        Some(no),
                        Some(no),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 7,
                        space,
                        register: slot,
                        dxil_kind: 11,
                    });
                }
                uavs = Some(self.m.md_node(list));
            }
            let mut samplers = None;
            if self.samplers > 0 {
                let ty = self.m.named_struct("struct.SamplerState", vec![self.t.i32]);
                let p = self.m.ptr(ty);
                let u = self.m.undef(p);
                let mut list = Vec::new();
                for n in 0..self.samplers {
                    let kind = (self.cmp_samplers >> n & 1) as u32;
                    let ops = vec![
                        Some(i32c(&mut self, n)),
                        Some(self.m.md_value(u)),
                        Some(empty),
                        Some(i32c(&mut self, SAMPLER_SPACE)),
                        Some(i32c(&mut self, n)),
                        Some(i32c(&mut self, 1)),
                        // Sampler kind: 1 comparison.
                        Some(i32c(&mut self, kind)),
                        None,
                    ];
                    list.push(Some(self.m.md_node(ops)));
                    bindings.push(Binding {
                        kind: 1,
                        space: SAMPLER_SPACE,
                        register: n,
                        dxil_kind: 14,
                    });
                }
                samplers = Some(self.m.md_node(list));
            }
            resources = Some(self.m.md_node(vec![srvs, uavs, cbvs, samplers]));
            // PSV0 lists them as the module does: CBVs, samplers, SRVs.
            bindings.sort_by_key(|b| match b.kind {
                2 => 0,
                1 => 1,
                _ => 2,
            });
        }
        if let Some(r) = resources {
            self.m.named_md("dx.resources", vec![r]);
        }

        let graphics = self.threads.is_none() || self.mesh.is_some();
        let (view, io_deps) = if graphics {
            self.view_id_state()
        } else {
            (Vec::new(), Vec::new())
        };
        if graphics {
            let arr = self.m.array(view.len() as u32, self.t.i32);
            let elems: Vec<V> = view.iter().map(|&v| self.m.i32(v)).collect();
            let arr = self.m.aggregate(arr, &elems);
            let arr = self.m.md_value(arr);
            let view_node = self.m.md_node(vec![Some(arr)]);
            self.m.named_md("dx.viewIdState", vec![view_node]);
        }

        if self.pixel {
            container::same_mode_per_row(&mut self.inputs, true);
        } else {
            container::same_mode_per_row(&mut self.outputs, false);
        }
        let sigs = if graphics {
            let ins = self.signature_md(self.inputs.clone());
            let outs = self.signature_md(self.outputs.clone());
            let prims = self.signature_md(self.prims.clone());
            Some(self.m.md_node(vec![ins, outs, prims]))
        } else {
            None
        };
        // A pixel shader with an SV_StencilRef output (as DXC collects it).
        if self.pixel && self.outputs.iter().any(|o| o.kind == 20) {
            self.flags |= flags::STENCIL_REF;
        }
        // Entry properties: shader flags (tag 0), thread-group size (4),
        // wave size (11).
        let mut props = Vec::new();
        if self.flags != 0 {
            props.push(i32c(&mut self, 0));
            let i64t = self.m.int(64);
            let f = self.m.int_const(i64t, self.flags as i64);
            props.push(self.m.md_value(f));
        }
        if let Some(threads) = self.threads {
            let t = threads.map(|n| Some(i32c(&mut self, n)));
            let t = self.m.md_node(t.to_vec());
            match self.mesh {
                // Mesh state (tag 9): threads, maximum vertices and
                // primitives, topology, payload bytes.
                Some(mesh) => {
                    props.push(i32c(&mut self, 9));
                    let state = [
                        mesh.max_vertices,
                        mesh.max_primitives,
                        mesh.topology.into(),
                        0,
                    ]
                    .map(|n| Some(i32c(&mut self, n)));
                    let mut node = vec![Some(t)];
                    node.extend(state);
                    props.push(self.m.md_node(node));
                }
                None => {
                    props.push(i32c(&mut self, 4));
                    props.push(t);
                }
            }
        }
        // Geometry state (tag 1): triangles in, maximum vertices, stream
        // mask, triangle strips out, one instance.
        if let Some(max) = self.gs {
            props.push(i32c(&mut self, 1));
            let state = [3, max, 1, 5, 1].map(|n| Some(i32c(&mut self, n)));
            props.push(self.m.md_node(state.to_vec()));
        }
        if let Some(n) = self.wave_size {
            props.push(i32c(&mut self, 11));
            let n = Some(i32c(&mut self, n));
            props.push(self.m.md_node(vec![n]));
        }
        let props =
            (!props.is_empty()).then(|| self.m.md_node(props.into_iter().map(Some).collect()));
        let main = self.m.md_value(self.main);
        let name = self.m.md_str("main");
        let entry = self
            .m
            .md_node(vec![Some(main), Some(name), sigs, resources, props]);

        let ident = self.m.md_str("victory");
        let ident = self.m.md_node(vec![Some(ident)]);
        self.m.named_md("llvm.ident", vec![ident]);
        let one = i32c(&mut self, 1);
        let six = i32c(&mut self, 6);
        let version = self.m.md_node(vec![Some(one), Some(six)]);
        self.m.named_md("dx.version", vec![version]);
        self.m.named_md("dx.valver", vec![version]);
        // Program kinds: 0 pixel, 1 vertex, 2 geometry, 5 compute, 13 mesh.
        let (name, kind) = match (self.threads, self.pixel) {
            _ if self.mesh.is_some() => ("ms", 13u32),
            _ if self.gs.is_some() => ("gs", 2),
            (Some(_), _) => ("cs", 5u32),
            (None, true) => ("ps", 0),
            (None, false) => ("vs", 1),
        };
        let name = self.m.md_str(name);
        let sm = self.m.md_node(vec![Some(name), Some(six), Some(six)]);
        self.m.named_md("dx.shaderModel", vec![sm]);
        self.m.named_md("dx.entryPoints", vec![entry]);

        let bitcode = self.m.write();
        let mut dxil = Vec::with_capacity(24 + bitcode.len());
        dxil.extend((kind << 16 | 0x66).to_le_bytes());
        dxil.extend((((24 + bitcode.len()) / 4) as u32).to_le_bytes());
        dxil.extend(b"DXIL");
        dxil.extend(0x106u32.to_le_bytes());
        dxil.extend(16u32.to_le_bytes());
        dxil.extend((bitcode.len() as u32).to_le_bytes());
        dxil.extend(&bitcode);

        let mut features = 0u64;
        if self.flags & flags::UAVS_EVERY_STAGE != 0 {
            features |= 0x4;
        }
        if self.flags & flags::STENCIL_REF != 0 {
            features |= 0x200;
        }
        if self.flags & flags::TYPED_UAV_LOADS != 0 {
            features |= 0x800;
        }
        if self.flags & flags::ROVS != 0 {
            features |= 0x1000;
        }
        if self.flags & flags::WAVE_OPS != 0 {
            features |= 0x4000;
        }
        if self.flags & flags::INT64 != 0 {
            features |= 0x8000;
        }
        if self.flags & flags::RESOURCE_HEAP != 0 {
            features |= 0x200_0000;
        }
        if self.flags & flags::SAMPLER_HEAP != 0 {
            features |= 0x400_0000;
        }
        if self.flags & flags::ATOMIC64_TYPED != 0 {
            features |= 0x40_0000;
        }
        if self.flags & flags::ATOMIC64_HEAP != 0 {
            features |= 0x1000_0000;
        }
        let sfi0 = features.to_le_bytes();
        let isg1 = container::signature(&self.inputs, true);
        let osg1 = container::signature(&self.outputs, false);
        let flag = if self.pixel {
            self.outputs.iter().any(|o| (17..=19).contains(&o.kind))
        } else {
            self.outputs.iter().any(|o| o.kind == 3)
        };
        let psv0 = container::psv(&Psv {
            stage: kind as u8,
            flag: flag && self.threads.is_none(),
            inputs: &self.inputs,
            outputs: &self.outputs,
            prims: &self.prims,
            mesh: self.mesh.map(|m| (m, self.lds_dwords * 4)),
            gs: self.gs,
            bindings: &bindings,
            threads: self.threads.unwrap_or_default(),
            wave_size: self.wave_size,
            io_deps: &io_deps,
        });
        let hash = [0u8; 20];
        let psg1 = container::signature(&self.prims, false);
        let mut parts: Vec<(&[u8; 4], &[u8])> =
            vec![(b"SFI0", &sfi0), (b"ISG1", &isg1), (b"OSG1", &osg1)];
        if self.mesh.is_some() {
            parts.push((b"PSG1", &psg1));
        }
        parts.extend([(b"PSV0", &psv0[..]), (b"HASH", &hash), (b"DXIL", &dxil)]);
        container::build(&parts)
    }

    /// The user-data buffer's annotation carries its size, known only now.
    fn fix_user_data_size(&mut self, size: u32) {
        let old = [self.m.i32(13), self.m.i32(u32::MAX)];
        let old = self.m.aggregate(self.t.props, &old);
        let new = [self.m.i32(13), self.m.i32(size)];
        let new = self.m.aggregate(self.t.props, &new);
        self.m.replace_const(old, new);
    }

    fn signature_md(&mut self, elements: Vec<SigElement>) -> Option<Md> {
        if elements.is_empty() {
            return None;
        }
        let mut list = Vec::new();
        for (id, e) in elements.iter().enumerate() {
            let c32 = |s: &mut Self, v: u32| {
                let c = s.m.i32(v);
                Some(s.m.md_value(c))
            };
            let c8 = |s: &mut Self, v: u8| {
                let c = s.m.i8(v);
                Some(s.m.md_value(c))
            };
            let idx = c32(self, e.index);
            let indices = Some(self.m.md_node(vec![idx]));
            let usage = if e.usage != 0 {
                let (tag, mask) = (c32(self, 3), c32(self, u32::from(e.usage)));
                Some(self.m.md_node(vec![tag, mask]))
            } else {
                None
            };
            let ops = vec![
                c32(self, id as u32),
                Some(self.m.md_str(&e.name)),
                c8(self, e.comp),
                c8(self, e.kind),
                indices,
                c8(self, e.interp),
                c32(self, 1),
                c8(self, e.cols),
                c32(self, e.row.unwrap_or(u32::MAX)),
                c8(self, if e.row.is_some() { e.col } else { u8::MAX }),
                usage,
            ];
            list.push(Some(self.m.md_node(ops)));
        }
        Some(self.m.md_node(list))
    }

    /// Computes `dx.viewIdState` words and the input-to-output dependency table
    /// shared with `PSV0`.
    fn view_id_state(&self) -> (Vec<u32>, Vec<u32>) {
        let (ni, no) = (sig_scalars(&self.inputs), sig_scalars(&self.outputs));
        let mut view = vec![ni, no];
        // Mesh shaders have no inputs: then the primitive outputs' count.
        if self.mesh.is_some() {
            view.push(sig_scalars(&self.prims));
        }
        let io_deps = if ni == 0 || no == 0 {
            Vec::new()
        } else if ni > 128 || no > 128 {
            conservative_view_deps(ni, no)
        } else {
            self.compute_view_deps(ni, no)
        };
        view.extend_from_slice(&io_deps);
        // A geometry shader has four streams, each its output count and
        // table; the other three empty.
        if self.gs.is_some() {
            view.extend([0, 0, 0]);
        }
        (view, io_deps)
    }

    /// Forward SSA + post-dominator control-dependence + global/LDS
    /// input-to-output dependency propagation over `main`.
    fn compute_view_deps(&self, ni: u32, no: u32) -> Vec<u32> {
        let n = self.m.block_count();
        let words = no.div_ceil(32) as usize;
        if n == 0 {
            return vec![0u32; ni as usize * words];
        }

        // 1. Forward reachability and RPO from block 0.
        let mut reachable = vec![false; n];
        let mut fwd_post = Vec::new();
        let mut stack = vec![(0usize, 0usize)];
        reachable[0] = true;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            top.1 += 1;
            let succ = match (self.m.block_term(b), k) {
                (Some(Term::Br(t)), 0) => Some(t),
                (Some(Term::CondBr(_, t, _)), 0) => Some(t),
                (Some(Term::CondBr(_, _, f)), 1) => Some(f),
                _ => None,
            };
            if let Some(s) = succ {
                if !reachable[s] {
                    reachable[s] = true;
                    stack.push((s, 0));
                }
            } else {
                fwd_post.push(b);
                stack.pop();
            }
        }
        fwd_post.reverse();
        let fwd_rpo = fwd_post;

        // 2. Build forward and reverse CFG with a virtual exit node `n` and
        // compute post-dominator control-dependence predecessors when `main`
        // contains conditional branches.
        let mut ctrl_preds = vec![Vec::new(); n];
        let has_cond_br = fwd_rpo
            .iter()
            .any(|&b| matches!(self.m.block_term(b), Some(Term::CondBr(..))));
        if has_cond_br {
            let exit = n;
            let mut fwd_succs = vec![Vec::new(); n + 1];
            let mut rev_succs = vec![Vec::new(); n + 1];
            for &b in &fwd_rpo {
                match self.m.block_term(b) {
                    Some(Term::Br(t)) => add_cfg_edge(&mut fwd_succs, &mut rev_succs, b, t),
                    Some(Term::CondBr(_, t, f)) => {
                        add_cfg_edge(&mut fwd_succs, &mut rev_succs, b, t);
                        add_cfg_edge(&mut fwd_succs, &mut rev_succs, b, f);
                    }
                    Some(Term::Ret) | None => add_cfg_edge(&mut fwd_succs, &mut rev_succs, b, exit),
                }
            }
            // Connect exitless loops conservatively to `exit` so post-dominators
            // are total over all reachable blocks.
            let mut reaches_exit = vec![false; n + 1];
            mark_reaches_exit(exit, &rev_succs, &mut reaches_exit);
            for &b in fwd_rpo.iter().rev() {
                if !reaches_exit[b] {
                    add_cfg_edge(&mut fwd_succs, &mut rev_succs, b, exit);
                    mark_reaches_exit(b, &rev_succs, &mut reaches_exit);
                }
            }

            // 3. Reverse RPO from `exit` and Cooper-Harvey-Kennedy immediate
            // post-dominators.
            let mut rev_seen = vec![false; n + 1];
            let mut rev_post = Vec::with_capacity(fwd_rpo.len() + 1);
            let mut stack = vec![(exit, 0usize)];
            rev_seen[exit] = true;
            while let Some(top) = stack.last_mut() {
                let (b, k) = *top;
                if k < rev_succs[b].len() {
                    top.1 += 1;
                    let s = rev_succs[b][k];
                    if !rev_seen[s] {
                        rev_seen[s] = true;
                        stack.push((s, 0));
                    }
                } else {
                    rev_post.push(b);
                    stack.pop();
                }
            }
            rev_post.reverse();
            let rev_rpo = rev_post;
            let mut rpo_idx = vec![usize::MAX; n + 1];
            for (idx, &b) in rev_rpo.iter().enumerate() {
                rpo_idx[b] = idx;
            }

            const UNDEF: usize = usize::MAX;
            let mut ipdom = vec![UNDEF; n + 1];
            ipdom[exit] = exit;
            let mut changed = true;
            while changed {
                changed = false;
                for &b in &rev_rpo[1..] {
                    let mut new_ipdom = UNDEF;
                    for &s in &fwd_succs[b] {
                        if ipdom[s] != UNDEF {
                            new_ipdom = if new_ipdom == UNDEF {
                                s
                            } else {
                                intersect_ipdom(new_ipdom, s, &ipdom, &rpo_idx)
                            };
                        }
                    }
                    if new_ipdom != UNDEF && ipdom[b] != new_ipdom {
                        ipdom[b] = new_ipdom;
                        changed = true;
                    }
                }
            }

            // 4. Control-dependence predecessors: for CondBr in `u`, walk the
            // post-dominator tree up from each arm until `ipdom[u]`.
            for &u in &fwd_rpo {
                if let Some(Term::CondBr(_, t, f)) = self.m.block_term(u) {
                    let stop = ipdom[u];
                    for start in [t, f] {
                        let mut cur = start;
                        while cur != stop && cur != exit && cur != UNDEF {
                            if !ctrl_preds[cur].contains(&u) {
                                ctrl_preds[cur].push(u);
                            }
                            let next = ipdom[cur];
                            if next == cur {
                                break;
                            }
                            cur = next;
                        }
                    }
                }
            }
        }

        // 5. Fixed-point SSA + phi-edge + global/LDS/resource dependency propagation.
        let live = self.m.live_insts();
        let mut dep_val = vec![0u128; self.m.inst_count()];
        let mut dep_block = vec![0u128; n];
        let mut dep_global = vec![0u128; self.m.global_count()];
        let mut dep_resource = 0u128;

        const MAX_ITERS: usize = 64;
        let mut converged = false;
        for _ in 0..MAX_ITERS {
            let mut step_changed = false;
            for &b in &fwd_rpo {
                let mut bd = dep_block[b];
                for &u in &ctrl_preds[b] {
                    bd |= dep_block[u];
                    if let Some(Term::CondBr(cond, ..)) = self.m.block_term(u) {
                        bd |= dep_of(cond, &dep_val);
                    }
                }
                if dep_block[b] != bd {
                    dep_block[b] = bd;
                    step_changed = true;
                }

                for &inst in self.m.block_phis(b) {
                    if !live[inst as usize] {
                        continue;
                    }
                    if let Op::Phi(incoming) = self.m.inst_op(inst) {
                        let mut d = 0u128;
                        for &(v, pred_bb) in incoming {
                            if pred_bb < n && reachable[pred_bb] {
                                d |= dep_of(v, &dep_val) | dep_block[pred_bb];
                                if let Some(Term::CondBr(cond, ..)) = self.m.block_term(pred_bb) {
                                    d |= dep_of(cond, &dep_val);
                                }
                            }
                        }
                        if dep_val[inst as usize] != d {
                            dep_val[inst as usize] = d;
                            step_changed = true;
                        }
                    }
                }

                for &inst in self.m.block_body(b) {
                    let d = match self.m.inst_op(inst) {
                        Op::Bin(_, x, y) | Op::Cmp(_, x, y) => {
                            dep_of(*x, &dep_val) | dep_of(*y, &dep_val)
                        }
                        Op::Cast(_, v) | Op::Extract(v, _) => dep_of(*v, &dep_val),
                        Op::Select(c, t, f) => {
                            dep_of(*c, &dep_val) | dep_of(*t, &dep_val) | dep_of(*f, &dep_val)
                        }
                        Op::Gep(_, p, idx) => idx
                            .iter()
                            .fold(dep_of(*p, &dep_val), |acc, &i| acc | dep_of(i, &dep_val)),
                        Op::Load(p) => dep_of(*p, &dep_val) | read_global(&self.m, *p, &dep_global),
                        Op::Store(p, v) => {
                            let wd = dep_of(*p, &dep_val) | dep_of(*v, &dep_val) | dep_block[b];
                            write_global(&self.m, *p, wd, &mut dep_global, &mut step_changed);
                            0
                        }
                        Op::AtomicRmw(_, p, v) => {
                            let wd = dep_of(*p, &dep_val) | dep_of(*v, &dep_val) | dep_block[b];
                            write_global(&self.m, *p, wd, &mut dep_global, &mut step_changed);
                            read_global(&self.m, *p, &dep_global)
                                | dep_of(*p, &dep_val)
                                | dep_of(*v, &dep_val)
                        }
                        Op::CmpXchg(p, exp, new) => {
                            let wd = dep_of(*p, &dep_val)
                                | dep_of(*exp, &dep_val)
                                | dep_of(*new, &dep_val)
                                | dep_block[b];
                            write_global(&self.m, *p, wd, &mut dep_global, &mut step_changed);
                            read_global(&self.m, *p, &dep_global)
                                | dep_of(*p, &dep_val)
                                | dep_of(*exp, &dep_val)
                                | dep_of(*new, &dep_val)
                        }
                        Op::Call(func, args) => {
                            let arg_dep =
                                args.iter().fold(0u128, |acc, &a| acc | dep_of(a, &dep_val));
                            if self.m.func_name(*func).starts_with("dx.op.") {
                                match args.first().and_then(|&v| self.m.int_value(v)) {
                                    // loadInput(4), evalSnapped(87), evalSampleIndex(88),
                                    // evalCentroid(89), attributeAtVertex(137)
                                    Some(4 | 87 | 88 | 89 | 137) => {
                                        let sig_id = args.get(1).and_then(|&v| self.m.int_value(v));
                                        let row_idx =
                                            args.get(2).and_then(|&v| self.m.int_value(v));
                                        let col_idx =
                                            args.get(3).and_then(|&v| self.m.int_value(v));
                                        sig_scalar_mask(&self.inputs, ni, sig_id, row_idx, col_idx)
                                            | arg_dep
                                    }
                                    // Resource writes: textureStore(67), bufferStore(69),
                                    // rawBufferStore(140), textureStoreSample(225).
                                    Some(67 | 69 | 140 | 225) => {
                                        let next = dep_resource | arg_dep | dep_block[b];
                                        if dep_resource != next {
                                            dep_resource = next;
                                            step_changed = true;
                                        }
                                        0
                                    }
                                    // Resource read-modify-write atomics: atomicBinOp(78),
                                    // atomicCompareExchange(79).
                                    Some(78 | 79) => {
                                        let next = dep_resource | arg_dep | dep_block[b];
                                        if dep_resource != next {
                                            dep_resource = next;
                                            step_changed = true;
                                        }
                                        arg_dep | dep_resource
                                    }
                                    // Resource reads with implicit quad derivatives:
                                    // sample(60), sampleBias(61), sampleCmp(64).
                                    Some(60 | 61 | 64) => arg_dep | dep_resource | dep_block[b],
                                    // Non-derivative resource reads:
                                    // sampleLevel(62), sampleGrad(63), sampleCmpLevelZero(65),
                                    // textureLoad(66), bufferLoad(68),
                                    // textureGather..=textureGatherCmp(73..=74), rawBufferLoad(139).
                                    Some(62 | 63 | 65 | 66 | 68 | 73..=74 | 139) => {
                                        arg_dep | dep_resource
                                    }
                                    // Cross-lane ops (CalculateLOD 81, DerivCoarse*/DerivFine*
                                    // 83..=86, Wave*/QuadReadLaneAt/QuadOp 110..=123,
                                    // WaveAllBitCount/WavePrefixBitCount 135..=136,
                                    // WaveMatch/WaveMultiPrefixOp/WaveMultiPrefixBitCount
                                    // 165..=167, IsHelperLane 221, QuadVote 222) depend on active-lane flow.
                                    Some(
                                        81 | 83..=86 | 110..=123 | 135..=136 | 165..=167 | 221..=222,
                                    ) => arg_dep | dep_block[b],
                                    // Known pure per-lane arithmetic, metadata, handle, output, and
                                    // barrier/discard intrinsics. Any unknown opcode falls back
                                    // conservatively to all input dependencies.
                                    Some(
                                        5 | 6..=59 | 72 | 80 | 82 | 93..=99 | 124..=134
                                        | 163..=164 | 168..=169 | 171..=172 | 216..=218,
                                    ) => arg_dep,
                                    _ => all_bits(ni),
                                }
                            } else {
                                arg_dep
                            }
                        }
                        Op::Phi(_) => 0,
                    };
                    if dep_val[inst as usize] != d {
                        dep_val[inst as usize] = d;
                        step_changed = true;
                    }
                }
            }
            if !step_changed {
                converged = true;
                break;
            }
        }
        if !converged {
            return conservative_view_deps(ni, no);
        }

        // 6. Collect discard, stream-emit, and storeOutput dependencies.
        let mut discard_dep = 0u128;
        let mut emit_dep = 0u128;
        for &b in &fwd_rpo {
            for &inst in self.m.block_body(b) {
                if let Op::Call(func, args) = self.m.inst_op(inst)
                    && self.m.func_name(*func).starts_with("dx.op.")
                {
                    match args.first().and_then(|&v| self.m.int_value(v)) {
                        Some(82) => {
                            discard_dep |=
                                args.get(1).map_or(0, |&v| dep_of(v, &dep_val)) | dep_block[b];
                        }
                        Some(97..=99) => {
                            emit_dep |=
                                args.get(1).map_or(0, |&v| dep_of(v, &dep_val)) | dep_block[b];
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut out_dep = vec![0u128; no as usize];
        for &b in &fwd_rpo {
            for &inst in self.m.block_body(b) {
                if let Op::Call(func, args) = self.m.inst_op(inst)
                    && self.m.func_name(*func).starts_with("dx.op.")
                    && args.first().and_then(|&v| self.m.int_value(v)) == Some(5)
                {
                    let sig_id = args.get(1).and_then(|&v| self.m.int_value(v));
                    let row_v = args.get(2).copied();
                    let col_v = args.get(3).copied();
                    let val_v = args.get(4).copied();
                    let d = row_v.map_or(0, |v| dep_of(v, &dep_val))
                        | col_v.map_or(0, |v| dep_of(v, &dep_val))
                        | val_v.map_or(0, |v| dep_of(v, &dep_val))
                        | dep_block[b]
                        | emit_dep;
                    let row_idx = row_v.and_then(|v| self.m.int_value(v));
                    let col_idx = col_v.and_then(|v| self.m.int_value(v));
                    let mut m = sig_scalar_mask(&self.outputs, no, sig_id, row_idx, col_idx);
                    while m != 0 {
                        let o = m.trailing_zeros() as usize;
                        if o < out_dep.len() {
                            out_dep[o] |= d;
                        }
                        m &= m - 1;
                    }
                }
            }
        }

        for d in &mut out_dep {
            *d |= discard_dep;
        }

        // Transpose `out_dep[0..no]` into `in_dep[0..ni]` packed `words` dwords/row.
        let mut table = vec![0u32; ni as usize * words];
        for (o, &mask) in out_dep.iter().enumerate() {
            let mut m = mask;
            while m != 0 {
                let i = m.trailing_zeros() as usize;
                if i < ni as usize {
                    table[i * words + o / 32] |= 1u32 << (o % 32);
                }
                m &= m - 1;
            }
        }
        table
    }
}

fn sig_scalars(els: &[SigElement]) -> u32 {
    els.iter()
        .filter_map(|e| e.row.map(|r| r * 4 + u32::from(e.col + e.cols)))
        .max()
        .unwrap_or(0)
}

fn conservative_view_deps(ni: u32, no: u32) -> Vec<u32> {
    let words = no.div_ceil(32);
    let mut deps = Vec::with_capacity((ni * words) as usize);
    for _ in 0..ni {
        for k in 0..words {
            let bits = (no - 32 * k).min(32);
            deps.push(if bits == 32 {
                u32::MAX
            } else {
                (1 << bits) - 1
            });
        }
    }
    deps
}

fn add_cfg_edge(fwd: &mut [Vec<usize>], rev: &mut [Vec<usize>], u: usize, v: usize) {
    if !fwd[u].contains(&v) {
        fwd[u].push(v);
    }
    if !rev[v].contains(&u) {
        rev[v].push(u);
    }
}

fn mark_reaches_exit(start: usize, rev_succs: &[Vec<usize>], reaches_exit: &mut [bool]) {
    let mut work = Vec::new();
    if !reaches_exit[start] {
        reaches_exit[start] = true;
        work.push(start);
    }
    while let Some(v) = work.pop() {
        for &p in &rev_succs[v] {
            if !reaches_exit[p] {
                reaches_exit[p] = true;
                work.push(p);
            }
        }
    }
}

fn intersect_ipdom(mut f1: usize, mut f2: usize, ipdom: &[usize], rpo_idx: &[usize]) -> usize {
    while f1 != f2 {
        while rpo_idx[f1] > rpo_idx[f2] {
            f1 = ipdom[f1];
        }
        while rpo_idx[f2] > rpo_idx[f1] {
            f2 = ipdom[f2];
        }
    }
    f1
}

fn dep_of(v: V, dep_val: &[u128]) -> u128 {
    match v {
        V::Inst(i) => dep_val[i as usize],
        _ => 0,
    }
}

fn resolve_global(m: &Module, mut v: V) -> Option<usize> {
    for _ in 0..64 {
        match v {
            V::Global(g) => return Some(g as usize),
            V::Inst(i) => match m.inst_op(i) {
                Op::Gep(_, base, _) | Op::Cast(_, base) => v = *base,
                _ => return None,
            },
            _ => return None,
        }
    }
    None
}

fn read_global(m: &Module, ptr: V, dep_global: &[u128]) -> u128 {
    match resolve_global(m, ptr) {
        Some(g) => dep_global[g],
        None => dep_global.iter().copied().fold(0, |a, b| a | b),
    }
}

fn write_global(m: &Module, ptr: V, d: u128, dep_global: &mut [u128], changed: &mut bool) {
    match resolve_global(m, ptr) {
        Some(g) => {
            let next = dep_global[g] | d;
            if dep_global[g] != next {
                dep_global[g] = next;
                *changed = true;
            }
        }
        None => {
            for slot in dep_global {
                let next = *slot | d;
                if *slot != next {
                    *slot = next;
                    *changed = true;
                }
            }
        }
    }
}

fn all_bits(limit: u32) -> u128 {
    if limit == 0 {
        0
    } else if limit >= 128 {
        u128::MAX
    } else {
        (1u128 << limit) - 1
    }
}

fn sig_scalar_mask(
    els: &[SigElement],
    limit: u32,
    sig_id: Option<i64>,
    row_idx: Option<i64>,
    col_idx: Option<i64>,
) -> u128 {
    let Some(elem) = sig_id
        .and_then(|id| usize::try_from(id).ok())
        .and_then(|i| els.get(i))
    else {
        return all_bits(limit);
    };
    let Some(base_row) = elem.row else {
        return 0;
    };
    if row_idx != Some(0) {
        return all_bits(limit);
    }
    if let Some(c) = col_idx
        && c >= 0
        && c < i64::from(elem.cols)
    {
        let idx = base_row * 4 + u32::from(elem.col) + c as u32;
        return if idx < limit { 1u128 << idx } else { 0 };
    }
    let mut mask = 0u128;
    for c in 0..u32::from(elem.cols) {
        let idx = base_row * 4 + u32::from(elem.col) + c;
        if idx < limit {
            mask |= 1u128 << idx;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::{Container, FourCC};

    fn sig(name: &str, kind: u8, interp: u8, row: u32, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp,
            row: Some(row),
            col: 0,
            cols,
            usage: (1 << cols) - 1,
            half: false,
        }
    }

    fn load_in(s: &mut Shader, block: usize, id: u32, col: u8) -> V {
        let args = [s.m.i32(id), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
        s.op(block, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args)
    }

    fn store_out(s: &mut Shader, block: usize, id: u32, col: u8, val: V) {
        let args = [s.m.i32(id), s.m.i32(0), s.m.i8(col), val];
        s.op(block, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
    }

    fn psv_io_deps(container_bytes: &[u8]) -> Vec<u32> {
        let c = Container::parse(container_bytes).unwrap();
        let psv = c.part(FourCC::PSV0).unwrap().data;
        let u32_at = |at: usize| u32::from_le_bytes(psv[at..at + 4].try_into().unwrap()) as usize;
        let info_size = u32_at(0);
        let in_count = psv[4 + 28] as usize;
        let out_count = psv[4 + 29] as usize;
        let prim_count = psv[4 + 30] as usize;
        let in_vectors = psv[4 + 31] as usize;
        let out_vectors = psv[4 + 32] as usize;
        let mut at = 4 + info_size;
        let res_count = u32_at(at);
        at += 4;
        if res_count > 0 {
            let res_size = u32_at(at);
            at += 4 + res_count * res_size;
        }
        let str_len = u32_at(at);
        at += 4 + str_len;
        let idx_count = u32_at(at);
        at += 4 + idx_count * 4;
        let sig_count = in_count + out_count + prim_count;
        if sig_count > 0 {
            let elem_size = u32_at(at);
            at += 4 + sig_count * elem_size;
        }
        let expected = if in_vectors > 0 && out_vectors > 0 {
            in_vectors * 4 * (out_vectors * 4).div_ceil(32)
        } else {
            0
        };
        assert_eq!(psv.len() - at, expected * 4);
        psv[at..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    }

    fn validate_dxil(bytes: &[u8]) {
        #[cfg(windows)]
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut bytes.to_vec())
                .expect("DXIL validation failed");
        }
        #[cfg(not(windows))]
        let _ = bytes;
    }

    fn build_sparse_vs() -> Shader {
        let mut s = Shader::new(false);
        let b = s.entry;
        s.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        s.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        let in0_x = load_in(&mut s, b, 0, 0);
        let in1_y = load_in(&mut s, b, 1, 1);
        let zero = s.m.f32(0.0f32.to_bits());
        store_out(&mut s, b, 0, 0, in0_x);
        store_out(&mut s, b, 0, 1, zero);
        store_out(&mut s, b, 0, 2, zero);
        store_out(&mut s, b, 0, 3, zero);
        store_out(&mut s, b, 1, 0, zero);
        store_out(&mut s, b, 1, 1, in1_y);
        s.m.terminate(b, Term::Ret);
        s
    }

    fn build_branch_vs() -> Shader {
        let mut s = Shader::new(false);
        let entry = s.entry;
        let then_bb = s.m.new_block();
        let else_bb = s.m.new_block();
        let merge_bb = s.m.new_block();

        s.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        s.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];

        let in0_x = load_in(&mut s, entry, 0, 0);
        let in1_x = load_in(&mut s, entry, 1, 0);
        let in1_y = load_in(&mut s, entry, 1, 1);
        let zero = s.m.f32(0.0f32.to_bits());
        let cond = s.m.cmp(entry, pred::OGT, in0_x, zero);
        s.m.terminate(entry, Term::CondBr(cond, then_bb, else_bb));

        s.m.terminate(then_bb, Term::Br(merge_bb));
        s.m.terminate(else_bb, Term::Br(merge_bb));

        let phi = s.m.phi(merge_bb, s.t.f32);
        s.m.set_phi(phi, vec![(in1_x, then_bb), (zero, else_bb)]);
        store_out(&mut s, merge_bb, 0, 0, phi);
        store_out(&mut s, merge_bb, 0, 1, zero);
        store_out(&mut s, merge_bb, 0, 2, zero);
        store_out(&mut s, merge_bb, 0, 3, zero);
        store_out(&mut s, merge_bb, 1, 0, zero);
        store_out(&mut s, merge_bb, 1, 1, in1_y);
        s.m.terminate(merge_bb, Term::Ret);
        s
    }

    fn build_loop_vs() -> Shader {
        let mut s = Shader::new(false);
        let entry = s.entry;
        let head = s.m.new_block();
        let body = s.m.new_block();
        let exit = s.m.new_block();

        s.inputs = vec![sig("POSITION", 0, 0, 0, 4)];
        s.outputs = vec![sig("SV_Position", 3, 0, 0, 4)];
        let in0_x = load_in(&mut s, entry, 0, 0);
        let in0_y = load_in(&mut s, entry, 0, 1);
        let in0_z = load_in(&mut s, entry, 0, 2);
        let zero_f = s.m.f32(0.0f32.to_bits());
        let as_int =
            s.m.cast(entry, super::super::bitcode::Cast::Bitcast, in0_x, s.t.i32);
        let limit_i =
            s.m.cast(entry, super::super::bitcode::Cast::Bitcast, in0_z, s.t.i32);
        s.m.terminate(entry, Term::Br(head));

        let acc = s.m.phi(head, s.t.i32);
        let cnt = s.m.phi(head, s.t.i32);
        let zero_i = s.m.i32(0);
        let one_i = s.m.i32(1);
        let cond = s.m.cmp(head, pred::IULT, cnt, limit_i);
        s.m.terminate(head, Term::CondBr(cond, body, exit));

        let next_acc = s.m.bin(body, super::super::bitcode::Bin::Add, acc, one_i);
        let next_cnt = s.m.bin(body, super::super::bitcode::Bin::Add, cnt, one_i);
        s.m.terminate(body, Term::Br(head));
        s.m.set_phi(acc, vec![(as_int, entry), (next_acc, body)]);
        s.m.set_phi(cnt, vec![(zero_i, entry), (next_cnt, body)]);

        let out_f =
            s.m.cast(exit, super::super::bitcode::Cast::Bitcast, acc, s.t.f32);
        store_out(&mut s, exit, 0, 0, out_f);
        store_out(&mut s, exit, 0, 1, in0_y);
        store_out(&mut s, exit, 0, 2, zero_f);
        store_out(&mut s, exit, 0, 3, zero_f);
        s.m.terminate(exit, Term::Ret);
        s
    }

    fn build_rect_gs_bytes() -> (Vec<u8>, Vec<u8>) {
        let mut vs = Shader::new(false);
        let b = vs.entry;
        vs.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        vs.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        for (id, cols) in [(0u32, 4u8), (1, 2)] {
            for col in 0..cols {
                let v = load_in(&mut vs, b, id, col);
                store_out(&mut vs, b, id, col, v);
            }
        }
        vs.m.terminate(b, Term::Ret);
        let vs_bytes = vs.finish();
        let gs_bytes = super::super::rect::rectangle_gs(&vs_bytes).unwrap();
        (vs_bytes, gs_bytes)
    }

    #[test]
    fn independent_channels_have_sparse_diagonal_deps() {
        let s = build_sparse_vs();

        let (view, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [1 << 0, 0, 0, 0, 0, 1 << 5]);
        assert_eq!(view, [6, 6, 1 << 0, 0, 0, 0, 0, 1 << 5]);

        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [1 << 0, 0, 0, 0, 0, 1 << 5, 0, 0]);
        validate_dxil(&bytes);
    }

    #[test]
    fn control_dependent_branch_tracks_condition_and_value() {
        let s = build_branch_vs();

        let (view, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [1 << 0, 0, 0, 0, 1 << 0, 1 << 5]);
        assert_eq!(view, [6, 6, 1 << 0, 0, 0, 0, 1 << 0, 1 << 5]);

        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [1 << 0, 0, 0, 0, 1 << 0, 1 << 5, 0, 0]);
        validate_dxil(&bytes);
    }

    #[test]
    fn pixel_discard_propagates_to_all_outputs() {
        let mut s = Shader::new(true);
        let b = s.entry;
        s.inputs = vec![sig("SV_Position", 3, 4, 0, 4), sig("TEXCOORD", 0, 2, 1, 2)];
        s.outputs = vec![sig("SV_Target", 16, 0, 0, 4), sig("SV_Target", 16, 0, 1, 2)];
        s.outputs[1].index = 1;

        let in0_z = load_in(&mut s, b, 0, 2);
        let in1_x = load_in(&mut s, b, 1, 0);
        let in1_y = load_in(&mut s, b, 1, 1);
        let zero = s.m.f32(0.0f32.to_bits());
        let kill = s.m.cmp(b, pred::OLT, in0_z, zero);
        s.op(b, "discard", Attr::NoUnwind, s.t.void, 82, &[kill]);
        store_out(&mut s, b, 0, 0, in1_x);
        store_out(&mut s, b, 0, 1, zero);
        store_out(&mut s, b, 0, 2, zero);
        store_out(&mut s, b, 0, 3, zero);
        store_out(&mut s, b, 1, 0, zero);
        store_out(&mut s, b, 1, 1, in1_y);
        s.m.terminate(b, Term::Ret);

        let (view, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [0, 0, 0x3F, 0, 1 << 0, 1 << 5]);
        assert_eq!(view, [6, 6, 0, 0, 0x3F, 0, 1 << 0, 1 << 5]);

        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [0, 0, 0x3F, 0, 1 << 0, 1 << 5, 0, 0]);
        validate_dxil(&bytes);
    }

    #[test]
    fn rectangle_gs_completes_right_angle_deps_and_validates() {
        let (vs_bytes, gs_bytes) = build_rect_gs_bytes();
        assert_eq!(psv_io_deps(&gs_bytes), [0x3F, 0x3F, 4, 8, 16, 32, 0, 0]);
        validate_dxil(&vs_bytes);
        validate_dxil(&gs_bytes);
    }

    #[test]
    fn loop_lds_and_exitless_loop_propagate_and_terminate() {
        // 1. Bounded loop phi + control-dependence propagation, validated by dxil.dll.
        let s = build_loop_vs();
        let (_, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [1 << 0, 1 << 1, 1 << 0, 0]);
        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [1 << 0, 1 << 1, 1 << 0, 0]);
        validate_dxil(&bytes);

        // 2. LDS Store/AtomicRmw/Load propagates dependencies through globals.
        let mut lds_s = Shader::new(false);
        lds_s.lds_dwords = 4;
        let lds = lds_s.lds();
        let b = lds_s.entry;
        lds_s.inputs = vec![sig("POSITION", 0, 0, 0, 4)];
        lds_s.outputs = vec![sig("SV_Position", 3, 0, 0, 4)];
        let lx = load_in(&mut lds_s, b, 0, 0);
        let ly = load_in(&mut lds_s, b, 0, 1);
        let idx0 = lds_s.m.i32(0);
        let ptr0 = lds_s.m.element(b, lds, idx0);
        let lx_i = lds_s
            .m
            .cast(b, super::super::bitcode::Cast::Bitcast, lx, lds_s.t.i32);
        let ly_i = lds_s
            .m
            .cast(b, super::super::bitcode::Cast::Bitcast, ly, lds_s.t.i32);
        lds_s.m.store(b, ptr0, lx_i);
        let old = lds_s
            .m
            .atomic(b, super::super::bitcode::Rmw::Add, ptr0, ly_i);
        let loaded = lds_s.m.load(b, ptr0);
        let sum = lds_s.m.bin(b, super::super::bitcode::Bin::Add, old, loaded);
        let sum_f = lds_s
            .m
            .cast(b, super::super::bitcode::Cast::Bitcast, sum, lds_s.t.f32);
        let zf = lds_s.m.f32(0.0f32.to_bits());
        store_out(&mut lds_s, b, 0, 0, sum_f);
        store_out(&mut lds_s, b, 0, 1, zf);
        store_out(&mut lds_s, b, 0, 2, zf);
        store_out(&mut lds_s, b, 0, 3, zf);
        lds_s.m.terminate(b, Term::Ret);
        let (_, lds_deps) = lds_s.view_id_state();
        assert_eq!(lds_deps, [1 << 0, 1 << 0, 0, 0]);

        // 3. Exitless loop CFG terminates cleanly in post-dominator construction.
        let mut inf = Shader::new(false);
        let e = inf.entry;
        let loop_bb = inf.m.new_block();
        inf.inputs = vec![sig("POSITION", 0, 0, 0, 2)];
        inf.outputs = vec![sig("SV_Position", 3, 0, 0, 2)];
        let x = load_in(&mut inf, e, 0, 0);
        inf.m.terminate(e, Term::Br(loop_bb));
        store_out(&mut inf, loop_bb, 0, 1, x);
        inf.m.terminate(loop_bb, Term::Br(loop_bb));
        let (_, inf_deps) = inf.view_id_state();
        assert_eq!(inf_deps, [1 << 1, 0]);
    }

    #[test]
    fn resource_store_to_load_and_atomic_propagate_input_deps() {
        // 1. Input written via rawBufferStore(140) and atomicBinOp(78) and read
        // back via rawBufferLoad(139) propagates input dependencies to outputs.
        let mut s = Shader::new(true);
        let b = s.entry;
        s.inputs = vec![sig("TEXCOORD", 0, 2, 0, 4)];
        s.outputs = vec![sig("SV_Target", 16, 0, 0, 4)];
        let in0_x = load_in(&mut s, b, 0, 0);
        let in0_y = load_in(&mut s, b, 0, 1);
        let in0_z = load_in(&mut s, b, 0, 2);
        let zero_f = s.m.f32(0.0f32.to_bits());
        let uav = s.root_uav_handle(0);
        let off = s.m.i32(0);
        let undef = s.m.undef(s.t.i32);
        let undef_f = s.m.undef(s.t.f32);
        let mask = s.m.i8(1);
        let align = s.m.i32(4);
        let y_i =
            s.m.cast(b, super::super::bitcode::Cast::Bitcast, in0_y, s.t.i32);
        s.op(
            b,
            "rawBufferStore.f32",
            Attr::NoUnwind,
            s.t.void,
            140,
            &[
                uav, off, undef, in0_x, undef_f, undef_f, undef_f, mask, align,
            ],
        );
        let add_op = s.m.i32(0);
        let prev = s.op(
            b,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            s.t.i32,
            78,
            &[uav, add_op, off, undef, undef, y_i],
        );
        let res = s.op(
            b,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            s.t.res_ret,
            139,
            &[uav, off, undef, mask, align],
        );
        let loaded = s.m.extract(b, res, 0);
        let loaded_i =
            s.m.cast(b, super::super::bitcode::Cast::Bitcast, loaded, s.t.i32);
        let sum = s.m.bin(b, super::super::bitcode::Bin::Add, prev, loaded_i);
        let out_x =
            s.m.cast(b, super::super::bitcode::Cast::Bitcast, sum, s.t.f32);
        store_out(&mut s, b, 0, 0, out_x);
        store_out(&mut s, b, 0, 1, in0_z);
        store_out(&mut s, b, 0, 2, zero_f);
        store_out(&mut s, b, 0, 3, zero_f);
        s.m.terminate(b, Term::Ret);

        let (_, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [1 << 0, 1 << 0, 1 << 1, 0]);
        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [1 << 0, 1 << 0, 1 << 1, 0]);
        validate_dxil(&bytes);

        // 2. Read-only rawBufferLoad(139) with no resource stores in the stage
        // keeps `dep_resource == 0` and introduces no spurious input dependencies.
        let mut ro = Shader::new(false);
        let b = ro.entry;
        ro.inputs = vec![sig("POSITION", 0, 0, 0, 4)];
        ro.outputs = vec![sig("SV_Position", 3, 0, 0, 4)];
        let _unused_x = load_in(&mut ro, b, 0, 0);
        let in0_y = load_in(&mut ro, b, 0, 1);
        let zero_f = ro.m.f32(0.0f32.to_bits());
        let srv = ro.root_srv_handle(0);
        let off = ro.m.i32(0);
        let undef = ro.m.undef(ro.t.i32);
        let mask = ro.m.i8(1);
        let align = ro.m.i32(4);
        let res = ro.op(
            b,
            "rawBufferLoad.f32",
            Attr::ReadOnly,
            ro.t.res_ret,
            139,
            &[srv, off, undef, mask, align],
        );
        let c0 = ro.m.extract(b, res, 0);
        store_out(&mut ro, b, 0, 0, c0);
        store_out(&mut ro, b, 0, 1, in0_y);
        store_out(&mut ro, b, 0, 2, zero_f);
        store_out(&mut ro, b, 0, 3, zero_f);
        ro.m.terminate(b, Term::Ret);

        let (_, ro_deps) = ro.view_id_state();
        assert_eq!(ro_deps, [0, 1 << 1, 0, 0]);
        let ro_bytes = ro.finish();
        assert_eq!(psv_io_deps(&ro_bytes), [0, 1 << 1, 0, 0]);
        validate_dxil(&ro_bytes);
    }

    #[test]
    fn cross_lane_opcodes_exclude_bitcasts_and_include_wave_and_derivatives() {
        // Test helper: runs opcode `op` with constant operands inside `then_bb`
        // (control-dependent on `in0_x > 0`), then stores its result in `merge_bb`
        // (which post-dominates `entry`, so `dep_block[merge_bb] == 0`).
        // Only opcodes classified as cross-lane pick up `dep_block[then_bb]`.
        let dep_for_opcode = |opcode: u32, name: &str| -> Vec<u32> {
            let mut s = Shader::new(false);
            let entry = s.entry;
            let then_bb = s.m.new_block();
            let else_bb = s.m.new_block();
            let merge_bb = s.m.new_block();
            s.inputs = vec![sig("POSITION", 0, 0, 0, 4)];
            s.outputs = vec![sig("SV_Position", 3, 0, 0, 4)];
            let in0_x = load_in(&mut s, entry, 0, 0);
            let zero_f = s.m.f32(0.0f32.to_bits());
            let one_f = s.m.f32(1.0f32.to_bits());
            let cond = s.m.cmp(entry, pred::OGT, in0_x, zero_f);
            s.m.terminate(entry, Term::CondBr(cond, then_bb, else_bb));
            let val = s.op(then_bb, name, Attr::ReadNone, s.t.f32, opcode, &[one_f]);
            s.m.terminate(then_bb, Term::Br(merge_bb));
            s.m.terminate(else_bb, Term::Br(merge_bb));
            store_out(&mut s, merge_bb, 0, 0, val);
            store_out(&mut s, merge_bb, 0, 1, zero_f);
            store_out(&mut s, merge_bb, 0, 2, zero_f);
            store_out(&mut s, merge_bb, 0, 3, zero_f);
            s.m.terminate(merge_bb, Term::Ret);
            s.view_id_state().1
        };

        // Bitcast and legacy conversion opcodes (124..=134) are per-lane and must NOT
        // pick up `dep_block[then_bb]`.
        for (op, name) in [
            (124, "bitcastI16toF16"),
            (127, "bitcastF32toI32"),
            (129, "bitcastF64toI64"),
            (134, "legacyDoubleToUInt32"),
        ] {
            assert_eq!(
                dep_for_opcode(op, name),
                [0, 0, 0, 0],
                "opcode {op} ({name})"
            );
        }

        // Derivative, wave, and quad opcodes depend on active-lane execution masks
        // and MUST include `dep_block[then_bb]`.
        for (op, name) in [
            (81, "calculateLOD"),
            (83, "derivCoarseX.f32"),
            (85, "derivFineX.f32"),
            (110, "waveIsFirstLane"),
            (119, "waveActiveOp.f32"),
            (123, "quadOp.f32"),
            (135, "waveAllBitCount"),
            (136, "wavePrefixBitCount"),
            (165, "waveMatch.f32"),
            (166, "waveMultiPrefixOp.f32"),
            (167, "waveMultiPrefixBitCount"),
            (221, "isHelperLane.i1"),
            (222, "quadVote"),
        ] {
            assert_eq!(
                dep_for_opcode(op, name),
                [1 << 0, 0, 0, 0],
                "opcode {op} ({name})"
            );
        }
    }

    #[test]
    fn dynamic_indexing_eval_implicit_sample_and_unknown_opcodes_are_conservative() {
        // 1. Dynamically-indexed `loadInput` (dynamic row) falls back to all input scalars,
        //    while dynamic column with `row_idx == 0` stays scoped to that element's columns.
        let mut s = Shader::new(false);
        let b = s.entry;
        s.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        s.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        let in0_x = load_in(&mut s, b, 0, 0);
        let dyn_idx = s.m.cast(b, super::super::bitcode::Cast::FpToSi, in0_x, s.t.i32);
        let zero_i = s.m.i32(0);
        let one_i = s.m.i32(1);
        let col0_i8 = s.m.i8(0);
        let zero_f = s.m.f32(0.0f32.to_bits());
        let undef = s.m.undef(s.t.i32);
        let args_row = [zero_i, dyn_idx, col0_i8, undef];
        let dyn_row_in = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args_row);
        let args_col = [one_i, zero_i, dyn_idx, undef];
        let dyn_col_in = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args_col);
        store_out(&mut s, b, 0, 0, dyn_row_in);
        store_out(&mut s, b, 0, 1, dyn_col_in);
        store_out(&mut s, b, 0, 2, zero_f);
        store_out(&mut s, b, 0, 3, zero_f);
        store_out(&mut s, b, 1, 0, zero_f);
        store_out(&mut s, b, 1, 1, zero_f);
        s.m.terminate(b, Term::Ret);
        let (_, deps) = s.view_id_state();
        // Out 0 (bit 0) depends on all 6 inputs; out 1 (bit 1) depends on POSITION.x (dyn_idx) + TEXCOORD (bits 4, 5).
        assert_eq!(deps, [0b11, 0b01, 0b01, 0b01, 0b11, 0b11]);

        // 2. Dynamically-indexed `storeOutput` (dynamic row) propagates to all outputs.
        let mut s_out = Shader::new(false);
        let b = s_out.entry;
        s_out.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        s_out.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        let x0 = load_in(&mut s_out, b, 0, 0);
        let y1 = load_in(&mut s_out, b, 1, 1);
        let dyn_out_row = s_out.m.cast(b, super::super::bitcode::Cast::FpToSi, x0, s_out.t.i32);
        let out_id0 = s_out.m.i32(0);
        let out_col0 = s_out.m.i8(0);
        s_out.op(
            b,
            "storeOutput.f32",
            Attr::NoUnwind,
            s_out.t.void,
            5,
            &[out_id0, dyn_out_row, out_col0, y1],
        );
        s_out.m.terminate(b, Term::Ret);
        let (_, out_deps) = s_out.view_id_state();
        assert_eq!(out_deps, [0x3F, 0, 0, 0, 0, 0x3F]);

        // 3. Attribute evaluation ops (evalSnapped 87, evalSampleIndex 88, evalCentroid 89,
        //    attributeAtVertex 137) propagate their input scalar and argument dependencies.
        for (op, name, extra) in [
            (87, "evalSnapped.f32", 2usize),
            (88, "evalSampleIndex.f32", 1),
            (89, "evalCentroid.f32", 0),
            (137, "attributeAtVertex.f32", 1),
        ] {
            let mut s = Shader::new(true);
            let b = s.entry;
            s.inputs = vec![sig("TEXCOORD", 0, 2, 0, 4)];
            s.outputs = vec![sig("SV_Target", 16, 0, 0, 4)];
            let arg_in = load_in(&mut s, b, 0, 3);
            let arg_i = s.m.cast(b, super::super::bitcode::Cast::FpToSi, arg_in, s.t.i32);
            let zero_f = s.m.f32(0.0f32.to_bits());
            let mut args = vec![s.m.i32(0), s.m.i32(0), s.m.i8(1)];
            for _ in 0..extra {
                args.push(arg_i);
            }
            let ev = s.op(b, name, Attr::ReadNone, s.t.f32, op, &args);
            store_out(&mut s, b, 0, 0, ev);
            store_out(&mut s, b, 0, 1, zero_f);
            store_out(&mut s, b, 0, 2, zero_f);
            store_out(&mut s, b, 0, 3, zero_f);
            s.m.terminate(b, Term::Ret);
            let want_inputs = if extra > 0 { (1 << 1) | (1 << 3) } else { 1 << 1 };
            let (_, ev_deps) = s.view_id_state();
            assert_eq!(
                ev_deps,
                [
                    0,
                    1,
                    0,
                    u32::from((want_inputs & (1 << 3)) != 0),
                ],
                "eval opcode {op} ({name})"
            );
        }

        // 4. Implicit-derivative samples (60, 61, 64) under a conditional branch include `dep_block[b]`
        //    and `dep_resource`, whereas explicit-level sampleLevel (62) without stores does not.
        let sample_dep = |opcode: u32, name: &str| -> Vec<u32> {
            let mut s = Shader::new(true);
            let entry = s.entry;
            let then_bb = s.m.new_block();
            let else_bb = s.m.new_block();
            let merge_bb = s.m.new_block();
            s.inputs = vec![sig("TEXCOORD", 0, 2, 0, 4)];
            s.outputs = vec![sig("SV_Target", 16, 0, 0, 4)];
            let in0_x = load_in(&mut s, entry, 0, 0);
            let zero_f = s.m.f32(0.0f32.to_bits());
            let cond = s.m.cmp(entry, pred::OGT, in0_x, zero_f);
            s.m.terminate(entry, Term::CondBr(cond, then_bb, else_bb));
            let res = s.op(then_bb, name, Attr::ReadOnly, s.t.res_ret, opcode, &[zero_f]);
            let texel = s.m.extract(then_bb, res, 0);
            s.m.terminate(then_bb, Term::Br(merge_bb));
            s.m.terminate(else_bb, Term::Br(merge_bb));
            store_out(&mut s, merge_bb, 0, 0, texel);
            store_out(&mut s, merge_bb, 0, 1, zero_f);
            store_out(&mut s, merge_bb, 0, 2, zero_f);
            store_out(&mut s, merge_bb, 0, 3, zero_f);
            s.m.terminate(merge_bb, Term::Ret);
            s.view_id_state().1
        };
        assert_eq!(sample_dep(60, "sample.f32"), [1, 0, 0, 0]);
        assert_eq!(sample_dep(61, "sampleBias.f32"), [1, 0, 0, 0]);
        assert_eq!(sample_dep(64, "sampleCmp.f32"), [1, 0, 0, 0]);
        assert_eq!(sample_dep(62, "sampleLevel.f32"), [0, 0, 0, 0]);

        // 5. Unknown `dx.op.*` opcode conservatively depends on all inputs.
        let mut s_unk = Shader::new(false);
        let b = s_unk.entry;
        s_unk.inputs = vec![sig("POSITION", 0, 0, 0, 4)];
        s_unk.outputs = vec![sig("SV_Position", 3, 0, 0, 4)];
        let _unused = load_in(&mut s_unk, b, 0, 0);
        let zero_f = s_unk.m.f32(0.0f32.to_bits());
        let unk = s_unk.op(b, "futureUnknownOp.f32", Attr::ReadNone, s_unk.t.f32, 999, &[zero_f]);
        store_out(&mut s_unk, b, 0, 0, unk);
        store_out(&mut s_unk, b, 0, 1, zero_f);
        store_out(&mut s_unk, b, 0, 2, zero_f);
        store_out(&mut s_unk, b, 0, 3, zero_f);
        s_unk.m.terminate(b, Term::Ret);
        let (_, unk_deps) = s_unk.view_id_state();
        assert_eq!(unk_deps, [1, 1, 1, 1]);
    }

    #[test]
    fn wave_op_in_branch_propagates_condition_and_operand_deps() {
        let mut s = Shader::new(false);
        s.flags |= flags::WAVE_OPS;
        let entry = s.entry;
        let then_bb = s.m.new_block();
        let else_bb = s.m.new_block();
        let merge_bb = s.m.new_block();
        s.inputs = vec![sig("POSITION", 0, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        s.outputs = vec![sig("SV_Position", 3, 0, 0, 4), sig("TEXCOORD", 0, 0, 1, 2)];
        let in0_x = load_in(&mut s, entry, 0, 0);
        let in0_y = load_in(&mut s, entry, 0, 1);
        let in0_z = load_in(&mut s, entry, 0, 2);
        let in1_y = load_in(&mut s, entry, 1, 1);
        let zero = s.m.f32(0.0f32.to_bits());
        let cond = s.m.cmp(entry, pred::OGT, in0_z, zero);
        s.m.terminate(entry, Term::CondBr(cond, then_bb, else_bb));
        let sum_op = s.m.i8(0);
        let signed_kind = s.m.i8(0);
        let wave_sum = s.op(
            then_bb,
            "waveActiveOp.f32",
            Attr::NoUnwind,
            s.t.f32,
            119,
            &[in0_x, sum_op, signed_kind],
        );
        s.m.terminate(then_bb, Term::Br(merge_bb));
        s.m.terminate(else_bb, Term::Br(merge_bb));
        let phi = s.m.phi(merge_bb, s.t.f32);
        s.m.set_phi(phi, vec![(wave_sum, then_bb), (zero, else_bb)]);
        store_out(&mut s, merge_bb, 0, 0, phi);
        store_out(&mut s, merge_bb, 0, 1, in0_y);
        store_out(&mut s, merge_bb, 0, 2, zero);
        store_out(&mut s, merge_bb, 0, 3, zero);
        store_out(&mut s, merge_bb, 1, 0, zero);
        store_out(&mut s, merge_bb, 1, 1, in1_y);
        s.m.terminate(merge_bb, Term::Ret);
        let (_, io_deps) = s.view_id_state();
        assert_eq!(io_deps, [1, 2, 1, 0, 0, 32]);
        let bytes = s.finish();
        assert_eq!(psv_io_deps(&bytes), [1, 2, 1, 0, 0, 32, 0, 0]);
        validate_dxil(&bytes);
    }

    /// A program that sets a bit through the write marks' UAV, or (`heap`)
    /// through a UAV indexed from the heap: a vertex (`pixel` false, no
    /// thread-group size), pixel or compute program.
    #[cfg(windows)]
    fn marked(pixel: bool, threads: Option<[u32; 3]>, heap: bool) -> Vec<u8> {
        let mut s = Shader::new(pixel);
        s.threads = threads;
        let b = s.entry;
        let h = if heap {
            let index = s.m.i32(0);
            s.heap_handle(b, index, false, false, (11 | 0x1000, 0))
        } else {
            s.view_writes_handle()
        };
        let (or, zero, one) = (s.m.i32(2), s.m.i32(0), s.m.i32(1));
        let iu = s.m.undef(s.t.i32);
        let i32t = s.t.i32;
        s.op(b, "atomicBinOp.i32", Attr::NoUnwind, i32t, 78, &[h, or, zero, iu, iu, one]);
        s.m.terminate(b, Term::Ret);
        s.finish()
    }

    /// A vertex program writing a declared UAV declares UAVs at every stage
    /// (the validator refuses it otherwise: "Flags must match usage", the
    /// 5,046 programs of the title's list); pixel and compute programs need
    /// not, nor a vertex program whose UAVs all come from the heap (the
    /// validator counts declared ranges only: 26 mesh and vertex programs).
    #[cfg(windows)]
    #[test]
    fn uavs_outside_pixel_and_compute_programs_are_declared() {
        let mut validator = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        )
        .ok();
        for (name, pixel, threads, heap) in [
            ("vertex", false, None, false),
            ("pixel", true, None, false),
            ("compute", false, Some([1, 1, 1]), false),
            ("vertex, heap", false, None, true),
        ] {
            let mut dxil = marked(pixel, threads, heap);
            if let Some(v) = validator.as_mut() {
                v.validate(&mut dxil)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        }
    }
}
