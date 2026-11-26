//! End-to-end tests of byte and half LDS accesses and of typed buffer
//! accesses: synthetic GFX10 compute programs lowered to DXIL, validated by
//! `dxil.dll` when present, run on the host GPU and WARP and checked
//! against a model of the console's memory.

use windows::Win32::Graphics::Dxgi::Common::*;

use super::host::{Dispatch, Host, Root, Stride, View};
use super::{HostShader, UserData, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const S_BARRIER: u32 = 0xBF8A_0000;
/// A literal operand follows the instruction.
const LIT: u16 = 255;
const VCC_LO: u8 = 106;
const EXEC_LO: u8 = 126;

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SMEM `s_load_dwordx4 s[dst:dst+3], s[base:base+1], offset`.
const fn s_load_dwordx4(dst: u8, base: u8, offset: u32) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25 | offset,
    ]
}

/// VOP1 `op vdst, src0` (`src0`: 0..105 SGPRs, 128.. inline, 256.. VGPRs).
const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

/// VOP2 `op vdst, src0, vsrc1`.
const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn v(n: u8) -> u16 {
    256 + n as u16
}

/// An inline integer constant (0..=64).
const fn int(n: u8) -> u16 {
    128 + n as u16
}

const V_MOV_B32: u16 = 0x01;
const V_MUL_U32_U24: u8 = 0x0B;
const V_LSHRREV_B32: u8 = 0x16;
const V_LSHLREV_B32: u8 = 0x1A;
const V_AND_B32: u8 = 0x1B;
const V_OR_B32: u8 = 0x1C;
const V_ADD_NC_U32: u8 = 0x25;

/// `v_mov_b32 vdst, literal`.
fn mov_lit(vdst: u8, x: u32) -> [u32; 2] {
    [vop1(V_MOV_B32, vdst, LIT), x]
}

/// A VOP2 with a literal `src0`.
fn vop2_lit(op: u8, vdst: u8, x: u32, vsrc1: u8) -> [u32; 2] {
    [vop2(op, vdst, LIT, vsrc1), x]
}

/// DS `op vdst, addr, data0 offset0 offset1`.
const fn ds(op: u8, vdst: u8, addr: u8, data0: u8, offset: u16) -> [u32; 2] {
    [
        0x36 << 26 | (op as u32) << 18 | offset as u32,
        (vdst as u32) << 24 | (data0 as u32) << 8 | addr as u32,
    ]
}

const DS_WRITE_B32: u8 = 0x0D;
const DS_WRITE_B8: u8 = 0x1E;
const DS_WRITE_B16: u8 = 0x1F;
const DS_READ_B32: u8 = 0x36;
const DS_READ_I8: u8 = 0x39;
const DS_READ_U8: u8 = 0x3A;
const DS_READ_I16: u8 = 0x3B;
const DS_READ_U16: u8 = 0x3C;
const DS_WRITE_B8_D16_HI: u8 = 0xA0;
const DS_WRITE_B16_D16_HI: u8 = 0xA1;
const DS_READ_U8_D16: u8 = 0xA2;
const DS_READ_U8_D16_HI: u8 = 0xA3;
const DS_READ_I8_D16: u8 = 0xA4;
const DS_READ_I8_D16_HI: u8 = 0xA5;
const DS_READ_U16_D16: u8 = 0xA6;
const DS_READ_U16_D16_HI: u8 = 0xA7;

/// MUBUF `op vdata, vaddr, s[srsrc:srsrc+3], soffset offset` with
/// `offen`/`idxen` (soffset 0x80: the inline constant 0).
#[allow(clippy::too_many_arguments)]
const fn mubuf(
    op: u8,
    vdata: u8,
    vaddr: u8,
    srsrc: u8,
    soffset: u8,
    offset: u16,
    offen: bool,
    idxen: bool,
) -> [u32; 2] {
    [
        0x38 << 26
            | (op as u32 & 0x7F) << 18
            | (op as u32 >> 7) << 25
            | (idxen as u32) << 13
            | (offen as u32) << 12
            | (offset as u32 & 0xFFF),
        (soffset as u32) << 24 | (srsrc as u32 >> 2) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

const BUFFER_STORE_DWORD: u8 = 0x1C;

/// `buffer_store_dword vdata, vaddr, s[srsrc:srsrc+3], 0 offen offset:...`.
const fn store_dword(vdata: u8, vaddr: u8, srsrc: u8, offset: u16) -> [u32; 2] {
    mubuf(
        BUFFER_STORE_DWORD,
        vdata,
        vaddr,
        srsrc,
        0x80,
        offset,
        true,
        false,
    )
}

/// A view table at user SGPR s0, starting at heap index 0.
fn table() -> RootParam {
    RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }
}

/// Table entry `n` (32 bytes each) of the table at s0.
fn entry(kind: ResourceType, n: u32, stride: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: 8 * n,
        lower_bound: n,
        stride,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }
}

/// Lowers a wave32 compute program of `threads` threads with `lds` bytes
/// of LDS, validates the DXIL when `dxil.dll` is there, and returns the
/// host shader, or the translation's refusal.
fn build(
    words: &[u32],
    threads: u32,
    lds: u32,
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
) -> Result<HostShader, String> {
    let user_sgprs: u32 = layout.iter().map(|p| p.sgprs).sum();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 64,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            rsrc2: user_sgprs << 1 | (lds / 512) << 15,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .map_err(|e| e.to_string())?;
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        memory: lowered.memory,
        memory_writes: lowered.memory_writes,
        view_addresses: lowered.view_addresses,
        heap: lowered.heap,
        heap_writes: lowered.heap_writes,
        view_marks: lowered.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    Ok(shader)
}

/// Runs one group of `shader` with `views` at heap indices 0.., user data
/// from `ud` (per entry), and `roots` after the user data (root parameter
/// 1..) on the hardware GPU and on WARP (each skipped without a device);
/// per one that ran, the UAVs' dwords.
fn run(
    shader: &HostShader,
    views: &[View],
    roots: &[Root],
    ud: &dyn Fn(&UserData) -> u32,
) -> Vec<(&'static str, Vec<Vec<u32>>)> {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut data = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            data[u.index as usize] = ud(u);
        }
        let mut params = vec![(0, Root::Constants(data))];
        for (n, r) in roots.iter().enumerate() {
            let Root::View { srv, data } = r else {
                panic!("root views only")
            };
            let r = Root::View {
                srv: *srv,
                data: data.clone(),
            };
            params.push((n as u32 + 1, r));
        }
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: params,
                    views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words));
    }
    assert!(!out.is_empty(), "neither hardware nor WARP SM 6.6 device");
    out
}

/// User data: the view table at heap index 0 (nothing else).
fn table_at_0(u: &UserData) -> u32 {
    assert_eq!(u.root, RootType::Table, "unexpected user data {u:?}");
    0
}

/// The prologue of the programs: the table address at s[4:5], its entry 0
/// at s[8:11] and entry 1 at s[12:15].
fn prologue() -> Vec<u32> {
    let mut w = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
    w.extend(s_load_dwordx4(8, 4, 0));
    w.extend(s_load_dwordx4(12, 4, 0x20));
    w
}

// ---- LDS bytes and halves ----

/// Dwords each lane stores.
const LDS_RESULTS: u32 = 32;
const LANES: u32 = 64;

/// What the d16 reads start from.
const D16_OLD: u32 = 0x9876_FEDC;

/// Lane `t`'s data registers.
fn lds_byte(t: u32) -> u32 {
    t * 37 + 0xABCD_EF85
}
fn lds_half(t: u32) -> u32 {
    t * 0x405 + 0x1234_7FF0
}
fn lds_high(t: u32) -> u32 {
    (t * 29 + 0x41) << 16 | 0xFF
}

/// 64 lanes (two waves) write bytes and halves of the same dwords, each
/// through its own lane's address; after a barrier, each reads them back
/// in every width and form.
fn lds_program() -> Vec<u32> {
    let mut w = prologue();
    // v1..v4: data; v6 = 2t, v7 = 4t, v8 = 0.
    w.extend(vop2_lit(V_MUL_U32_U24, 1, 37, 0));
    w.extend(vop2_lit(V_ADD_NC_U32, 1, 0xABCD_EF85, 1));
    w.extend(vop2_lit(V_MUL_U32_U24, 2, 0x405, 0));
    w.extend(vop2_lit(V_ADD_NC_U32, 2, 0x1234_7FF0, 2));
    w.extend(vop2_lit(V_MUL_U32_U24, 3, 29, 0));
    w.extend(vop2_lit(V_ADD_NC_U32, 3, 0x41, 3));
    w.push(vop2(V_LSHLREV_B32, 3, int(16), 3));
    w.extend(vop2_lit(V_OR_B32, 3, 0xFF, 3));
    // v4 = 1 << (t & 7): one bit per lane, so values merged by OR show.
    w.push(vop2(V_AND_B32, 4, int(7), 0));
    w.push(vop1(V_MOV_B32, 5, int(1)));
    w.push(vop2(V_LSHLREV_B32, 4, v(4), 5));
    w.push(vop2(V_LSHLREV_B32, 6, int(1), 0));
    w.push(vop2(V_LSHLREV_B32, 7, int(2), 0));
    w.push(vop1(V_MOV_B32, 8, int(0)));
    w.extend(mov_lit(9, 0xC3C3_C3C3));
    // Bytes 0..64: one per lane; halves at 64..192; the data's high
    // halves' bytes at 192..256 and halves at 256..384.
    w.extend(ds(DS_WRITE_B8, 0, 0, 1, 0));
    w.extend(ds(DS_WRITE_B16, 0, 6, 2, 64));
    w.extend(ds(DS_WRITE_B8_D16_HI, 0, 0, 3, 192));
    w.extend(ds(DS_WRITE_B16_D16_HI, 0, 6, 3, 256));
    // Every lane writes byte 384 (a bit each), byte 385 and half 386.
    w.extend(ds(DS_WRITE_B8, 0, 8, 4, 384));
    w.extend(ds(DS_WRITE_B8, 0, 8, 1, 385));
    w.extend(ds(DS_WRITE_B16, 0, 8, 2, 386));
    // Dword 512 + 4t: 0xC3C3C3C3, then (even lanes only) byte
    // (t >> 1) & 3 of it is t.
    w.extend(ds(DS_WRITE_B32, 0, 7, 9, 512));
    w.push(vop2(V_AND_B32, 10, int(1), 0));
    w.push(0x7D84_0000 | 10 << 9 | int(0) as u32); // v_cmp_eq_u32 vcc_lo, 0, v10
    w.push(0xBE80_3C00 | 20 << 16 | VCC_LO as u32); // s_and_saveexec_b32 s20, vcc_lo
    // (No s_cbranch_execz: the write is guarded by EXEC alone.)
    w.push(vop2(V_LSHRREV_B32, 11, int(1), 0));
    w.push(vop2(V_AND_B32, 11, int(3), 11));
    w.push(vop2(V_ADD_NC_U32, 11, v(7), 11));
    w.extend(ds(DS_WRITE_B8, 0, 11, 0, 512));
    w.push(0x8800_0000 | (EXEC_LO as u32) << 16 | 20 << 8 | EXEC_LO as u32); // s_or_b32 exec_lo, exec_lo, s20
    w.push(S_BARRIER);
    // v12 = 4 (t & 15), v13 = 4 (t & 31), v14 = the lane's results.
    w.push(vop2(V_AND_B32, 12, int(15), 0));
    w.push(vop2(V_LSHLREV_B32, 12, int(2), 12));
    w.push(vop2(V_AND_B32, 13, int(31), 0));
    w.push(vop2(V_LSHLREV_B32, 13, int(2), 13));
    w.push(vop2(V_LSHLREV_B32, 14, int(7), 0));
    for r in 28..34 {
        w.extend(mov_lit(r, D16_OLD));
    }
    let reads: [(u8, u8, u8, u16); 17] = [
        (DS_READ_B32, 20, 12, 0),
        (DS_READ_U8, 21, 0, 0),
        (DS_READ_I8, 22, 0, 0),
        (DS_READ_U16, 23, 6, 64),
        (DS_READ_I16, 24, 6, 64),
        (DS_READ_B32, 25, 13, 64),
        (DS_READ_U8, 26, 0, 192),
        (DS_READ_U16, 27, 6, 256),
        (DS_READ_U8_D16, 28, 0, 0),
        (DS_READ_U8_D16_HI, 29, 0, 0),
        (DS_READ_I8_D16, 30, 0, 0),
        (DS_READ_I8_D16_HI, 31, 0, 0),
        (DS_READ_U16_D16, 32, 6, 64),
        (DS_READ_U16_D16_HI, 33, 6, 64),
        (DS_READ_B32, 34, 8, 384),
        (DS_READ_B32, 35, 7, 512),
        (DS_READ_I16, 36, 6, 256),
    ];
    for (op, dst, addr, offset) in reads {
        w.extend(ds(op, dst, addr, 0, offset));
    }
    for k in 0..reads.len() as u8 {
        w.extend(store_dword(20 + k, 14, 8, 4 * u16::from(k)));
    }
    w.push(S_ENDPGM);
    w
}

/// The console's LDS after the writes, but for the bytes all lanes write
/// (384..388), checked apart.
fn lds_model() -> Vec<u8> {
    let mut m = vec![0u8; 1024];
    for t in 0..LANES {
        let at = t as usize;
        m[at] = lds_byte(t) as u8;
        m[64 + 2 * at..66 + 2 * at].copy_from_slice(&(lds_half(t) as u16).to_le_bytes());
        m[192 + at] = (lds_high(t) >> 16) as u8;
        m[256 + 2 * at..258 + 2 * at].copy_from_slice(&((lds_high(t) >> 16) as u16).to_le_bytes());
        m[512 + 4 * at..516 + 4 * at].copy_from_slice(&0xC3C3_C3C3u32.to_le_bytes());
        if t % 2 == 0 {
            m[512 + 4 * at + (at >> 1 & 3)] = t as u8;
        }
    }
    m
}

/// What lane `t` reads, in its result order (the shared dword at 384 apart).
fn lds_expected(m: &[u8], t: u32) -> Vec<u32> {
    let t = t as usize;
    let dword = |at: usize| u32::from_le_bytes(m[at..at + 4].try_into().unwrap());
    let half = |at: usize| u16::from_le_bytes([m[at], m[at + 1]]);
    let u8_ = u32::from(m[t]);
    let i8_ = m[t] as i8 as i32 as u32;
    let u16_ = u32::from(half(64 + 2 * t));
    let i16_ = half(64 + 2 * t) as i16 as i32 as u32;
    let lo = |x: u32| D16_OLD & 0xFFFF_0000 | x & 0xFFFF;
    let hi = |x: u32| D16_OLD & 0xFFFF | x << 16;
    vec![
        dword(4 * (t & 15)),
        u8_,
        i8_,
        u16_,
        i16_,
        dword(64 + 4 * (t & 31)),
        u32::from(m[192 + t]),
        u32::from(half(256 + 2 * t)),
        lo(u8_),
        hi(u8_),
        lo(i8_),
        hi(i8_ & 0xFFFF),
        lo(u16_),
        hi(u16_),
        0, // (the shared dword, checked apart)
        dword(512 + 4 * t),
        half(256 + 2 * t) as i16 as i32 as u32,
    ]
}

#[test]
fn lds_bytes_and_halves_of_one_dword_from_many_lanes() {
    let words = lds_program();
    let shader = build(
        &words,
        LANES,
        1024,
        vec![table()],
        vec![entry(ResourceType::Uav, 0, 0)],
    )
    .expect("lower");
    let views = [View::RawUav(LANES * LDS_RESULTS)];
    let m = lds_model();
    let bytes: Vec<u32> = (0..LANES).map(|t| lds_byte(t) & 0xFF).collect();
    let halves: Vec<u32> = (0..LANES).map(|t| lds_half(t) & 0xFFFF).collect();
    for (label, uavs) in run(&shader, &views, &[], &table_at_0) {
        let out = &uavs[0];
        for t in 0..LANES {
            let at = (t * LDS_RESULTS) as usize;
            let got = &out[at..at + 17];
            let want = lds_expected(&m, t);
            for (k, (g, w)) in got.iter().zip(&want).enumerate() {
                if k != 14 {
                    assert_eq!(g, w, "{label}: lane {t} result {k}: {g:#x}, want {w:#x}");
                }
            }
            // One lane's whole value wins in each shared byte and half;
            // every lane sees the same.
            let shared = got[14];
            assert_eq!(shared, out[14], "{label}: lane {t} shared dword");
            assert!(
                (shared & 0xFF).is_power_of_two(),
                "{label}: byte 384 {shared:#x} merges lanes' bits"
            );
            assert!(
                bytes.contains(&(shared >> 8 & 0xFF)),
                "{label}: byte 385 {shared:#x}"
            );
            assert!(
                halves.contains(&(shared >> 16)),
                "{label}: half 386 {shared:#x}"
            );
        }
    }
}

#[test]
fn sub_dword_lds_opcodes_decode() {
    let names = [
        "ds_read_u8",
        "ds_read_i8",
        "ds_read_u16",
        "ds_read_i16",
        "ds_read_u8_d16",
        "ds_read_u8_d16_hi",
        "ds_read_i8_d16",
        "ds_read_i8_d16_hi",
        "ds_read_u16_d16",
        "ds_read_u16_d16_hi",
        "ds_write_b8",
        "ds_write_b16",
        "ds_write_b8_d16_hi",
        "ds_write_b16_d16_hi",
    ];
    for n in names {
        assert!(lower::sub_dword(n), "{n}");
    }
    for n in [
        "ds_read_b32",
        "ds_read2_b32",
        "ds_write_b32",
        "ds_write2st64_b32",
        "ds_write_addtid_b32",
    ] {
        assert!(!lower::sub_dword(n), "{n}");
    }
}

// ---- typed buffers ----

const BUFFER_LOAD_FORMAT_XYZW: u8 = 0x03;
const BUFFER_STORE_FORMAT_XYZW: u8 = 0x07;
const V_CVT_F32_U32: u16 = 0x06;
const V_ADD_F32: u8 = 0x03;
const V_MUL_F32: u8 = 0x08;
/// Inline constants 0.5, 1.0 and 2.0.
const HALF: u16 = 240;
const ONE: u16 = 242;
const TWO: u16 = 244;

/// A float exactly representable as a half (normal or zero).
fn f16(x: f32) -> u16 {
    if x == 0.0 {
        return 0;
    }
    let b = x.to_bits();
    let (sign, exp, mant) = (b >> 31, (b >> 23 & 0xFF) as i32 - 127 + 15, b & 0x7F_FFFF);
    assert!(
        (1..31).contains(&exp) && mant & 0x1FFF == 0,
        "{x} is not a half"
    );
    (sign << 15 | (exp as u32) << 10 | mant >> 13) as u16
}

/// A half's value (normal or zero).
fn from_f16(h: u16) -> f32 {
    let (sign, exp, mant) = (
        u32::from(h >> 15),
        u32::from(h >> 10 & 0x1F),
        u32::from(h & 0x3FF),
    );
    if exp == 0 && mant == 0 {
        return f32::from_bits(sign << 31);
    }
    assert!((1..31).contains(&exp), "{h:#x}");
    f32::from_bits(sign << 31 | (exp + 112) << 23 | mant << 13)
}

fn channel(element: u32, c: u32) -> f32 {
    (element * 4 + c) as f32 + 0.5
}

/// Typed views in two formats: (format, bytes per element, the dwords of
/// `elements` elements, channel `c` of element `e` `channel(e, c)`).
fn typed_formats(elements: u32) -> Vec<(DXGI_FORMAT, u32, Vec<u32>)> {
    let mut full = Vec::new();
    let mut halves = Vec::new();
    for e in 0..elements {
        let ch: Vec<f32> = (0..4).map(|c| channel(e, c)).collect();
        full.extend(ch.iter().map(|x| x.to_bits()));
        for pair in ch.chunks(2) {
            halves.push(u32::from(f16(pair[0])) | u32::from(f16(pair[1])) << 16);
        }
    }
    vec![
        (DXGI_FORMAT_R32G32B32A32_FLOAT, 16, full),
        (DXGI_FORMAT_R16G16B16A16_FLOAT, 8, halves),
    ]
}

/// Channel `c` of element `e` of a typed view's dwords.
fn typed_channel(dwords: &[u32], size: u32, e: u32, c: u32) -> f32 {
    if size == 16 {
        f32::from_bits(dwords[(4 * e + c) as usize])
    } else {
        let d = dwords[(2 * e + c / 2) as usize];
        from_f16((d >> (16 * (c % 2))) as u16)
    }
}

/// Lowers a 32-thread `words` accessing the typed view at table entry 1
/// (`size`: the entry's stride) and writing results through entry 0.
fn build_typed(words: &[u32], size: u32, kind: ResourceType) -> Result<HostShader, String> {
    build(
        words,
        32,
        0,
        vec![table()],
        vec![entry(ResourceType::Uav, 0, 0), entry(kind, 1, size)],
    )
}

/// The elements the loads of `typed_load_program` read, per lane.
fn typed_load_elements(t: u32) -> [u32; 4] {
    [t + 1, t + 2 * (t & 1), t + 1, t + 3]
}

/// 32 lanes load whole elements of the typed view at entry 1 (elements
/// of `size` bytes) at byte offsets added the four ways the console adds
/// them: the instruction's offset to an index; a lane's offset to its
/// index; a lane's offset plus a constant SGPR offset; a lane's offset
/// plus the instruction's.
fn typed_load_program(size: u32) -> Vec<u32> {
    let mut w = prologue();
    // v1 = t, v2 = 2 size (t & 1), v3 = size t, v14 = 64 t.
    w.push(vop1(V_MOV_B32, 1, v(0)));
    w.push(vop2(V_AND_B32, 2, int(1), 0));
    w.extend(vop2_lit(V_MUL_U32_U24, 2, 2 * size, 2));
    w.extend(vop2_lit(V_MUL_U32_U24, 3, size, 0));
    w.push(vop2(V_LSHLREV_B32, 14, int(6), 0));
    let (op, s) = (BUFFER_LOAD_FORMAT_XYZW, size as u16);
    w.extend(mubuf(op, 20, 0, 12, 0x80, s, false, true));
    w.extend(mubuf(op, 24, 1, 12, 0x80, 0, true, true));
    w.extend(mubuf(op, 28, 3, 12, int(size as u8) as u8, 0, true, false));
    w.extend(mubuf(op, 32, 3, 12, 0x80, 3 * s, true, false));
    for k in 0..16u8 {
        w.extend(store_dword(20 + k, 14, 8, 4 * u16::from(k)));
    }
    w.push(S_ENDPGM);
    w
}

#[test]
fn typed_loads_at_whole_element_offsets() {
    for (format, size, data) in typed_formats(40) {
        let shader = build_typed(&typed_load_program(size), size, ResourceType::Srv)
            .unwrap_or_else(|e| panic!("{size}-byte elements: {e}"));
        let views = [
            View::RawUav(32 * 16),
            View::Buffer {
                data: &data,
                stride: Stride::Format(format, size),
            },
        ];
        for (label, uavs) in run(&shader, &views, &[], &table_at_0) {
            for t in 0..32 {
                for (k, e) in typed_load_elements(t).into_iter().enumerate() {
                    for c in 0..4 {
                        let at = (t * 16 + 4 * k as u32 + c) as usize;
                        let got = f32::from_bits(uavs[0][at]);
                        assert_eq!(
                            got,
                            channel(e, c),
                            "{label}, {size}-byte elements: lane {t} load {k} channel {c}"
                        );
                    }
                }
            }
        }
    }
}

/// 32 lanes store whole elements at byte offsets: element t + 32 through
/// an index and the instruction's offset, element t through a lane's
/// offset.
fn typed_store_program(size: u32) -> Vec<u32> {
    let mut w = prologue();
    w.push(vop1(V_CVT_F32_U32, 20, v(0)));
    w.push(vop2(V_ADD_F32, 21, HALF, 20));
    w.push(vop2(V_MUL_F32, 22, TWO, 20));
    w.push(vop2(V_ADD_F32, 23, ONE, 20));
    w.extend(vop2_lit(V_MUL_U32_U24, 3, size, 0));
    let op = BUFFER_STORE_FORMAT_XYZW;
    w.extend(mubuf(op, 20, 0, 12, 0x80, 32 * size as u16, false, true));
    w.push(vop2(V_ADD_F32, 20, HALF, 20));
    w.extend(mubuf(op, 20, 3, 12, 0x80, 0, true, false));
    w.push(S_ENDPGM);
    w
}

#[test]
fn typed_stores_at_whole_element_offsets() {
    for (format, size, data) in typed_formats(64) {
        let shader = build_typed(&typed_store_program(size), size, ResourceType::Uav)
            .unwrap_or_else(|e| panic!("{size}-byte elements: {e}"));
        let views = [
            View::RawUav(1),
            View::FormatUav {
                data: &data,
                format,
                bytes: size,
            },
        ];
        for (label, uavs) in run(&shader, &views, &[], &table_at_0) {
            for e in 0..64u32 {
                let t = (e % 32) as f32;
                let x = if e < 32 { t + 0.5 } else { t };
                let want = [x, t + 0.5, 2.0 * t, t + 1.0];
                for (c, &w) in want.iter().enumerate() {
                    let g = typed_channel(&uavs[1], size, e, c as u32);
                    assert_eq!(
                        g, w,
                        "{label}, {size}-byte elements: element {e} channel {c}"
                    );
                }
            }
        }
    }
}

#[test]
fn typed_offsets_refused_inside_an_element_or_of_unknown_size() {
    let load = |offset: u16| {
        let mut w = prologue();
        w.extend(mubuf(
            BUFFER_LOAD_FORMAT_XYZW,
            20,
            0,
            12,
            0x80,
            offset,
            false,
            true,
        ));
        w.extend(store_dword(20, 0, 8, 0));
        w.push(S_ENDPGM);
        w
    };
    let e = build_typed(&load(4), 16, ResourceType::Srv).expect_err("refused");
    assert!(e.contains("inside an element"), "{e}");
    let e = build_typed(&load(16), 0, ResourceType::Srv).expect_err("refused");
    assert!(e.contains("unknown element size"), "{e}");
    // Offset 0 needs no element size.
    build_typed(&load(0), 0, ResourceType::Srv).expect("offset 0");
}

const BUFFER_STORE_FORMAT_X: u8 = 0x04;
const BUFFER_STORE_FORMAT_XY: u8 = 0x05;
const BUFFER_STORE_FORMAT_XYZ: u8 = 0x06;
const BUFFER_STORE_FORMAT_D16_XY: u8 = 0x85;
/// The halves 1.0 (low) and 5.0 (high).
const D16_PAIR: u32 = 0x4500_3C00;

/// 32 lanes store some channels of elements 4t.. of the typed view at
/// entry 1: x of 4t, xy of 4t + 1, xyz of 4t + 2 (even lanes only, by EXEC
/// alone), and two halves of 4t + 3.
fn partial_store_program(size: u32) -> Vec<u32> {
    let mut w = prologue();
    w.push(vop1(V_CVT_F32_U32, 20, v(0)));
    w.push(vop2(V_ADD_F32, 21, HALF, 20));
    w.push(vop2(V_MUL_F32, 22, TWO, 20));
    w.extend(mov_lit(24, D16_PAIR));
    w.push(vop2(V_LSHLREV_B32, 1, int(2), 0));
    let s = size as u16;
    w.extend(mubuf(
        BUFFER_STORE_FORMAT_X,
        20,
        1,
        12,
        0x80,
        0,
        false,
        true,
    ));
    w.extend(mubuf(
        BUFFER_STORE_FORMAT_XY,
        20,
        1,
        12,
        0x80,
        s,
        false,
        true,
    ));
    w.extend(mubuf(
        BUFFER_STORE_FORMAT_D16_XY,
        24,
        1,
        12,
        0x80,
        3 * s,
        false,
        true,
    ));
    w.push(vop2(V_AND_B32, 10, int(1), 0));
    w.push(0x7D84_0000 | 10 << 9 | int(0) as u32); // v_cmp_eq_u32 vcc_lo, 0, v10
    w.push(0xBE80_3C00 | 20 << 16 | VCC_LO as u32); // s_and_saveexec_b32 s20, vcc_lo
    w.extend(mubuf(
        BUFFER_STORE_FORMAT_XYZ,
        20,
        1,
        12,
        0x80,
        2 * s,
        false,
        true,
    ));
    w.push(0x8800_0000 | (EXEC_LO as u32) << 16 | 20 << 8 | EXEC_LO as u32); // s_or_b32 exec_lo, exec_lo, s20
    w.push(S_ENDPGM);
    w
}

#[test]
fn typed_stores_of_some_channels_keep_the_others() {
    for (format, size, data) in typed_formats(128) {
        let shader = build_typed(&partial_store_program(size), size, ResourceType::Uav)
            .unwrap_or_else(|e| panic!("{size}-byte elements: {e}"));
        let views = [
            View::RawUav(1),
            View::FormatUav {
                data: &data,
                format,
                bytes: size,
            },
        ];
        for (label, uavs) in run(&shader, &views, &[], &table_at_0) {
            for e in 0..128u32 {
                let t = (e / 4) as f32;
                let given: &[f32] = match e % 4 {
                    0 => &[t],
                    1 => &[t, t + 0.5],
                    2 if e / 4 % 2 == 0 => &[t, t + 0.5, 2.0 * t],
                    2 => &[],
                    _ => &[1.0, 5.0],
                };
                for c in 0..4 {
                    let want = given.get(c as usize).copied().unwrap_or(channel(e, c));
                    let got = typed_channel(&uavs[1], size, e, c);
                    assert_eq!(
                        got, want,
                        "{label}, {size}-byte elements: element {e} channel {c}"
                    );
                }
            }
        }
    }
}

// ---- typed-format loads by index ----

/// MTBUF `tbuffer_load_format_* vdata, vaddr, s[srsrc:srsrc+3], 0
/// format:... offset` with `offen`/`idxen`.
#[allow(clippy::too_many_arguments)]
const fn mtbuf(
    op: u8,
    format: u8,
    vdata: u8,
    vaddr: u8,
    srsrc: u8,
    offset: u16,
    offen: bool,
    idxen: bool,
) -> [u32; 2] {
    [
        0x3A << 26
            | (format as u32 & 0x7F) << 19
            | (op as u32 & 7) << 16
            | (idxen as u32) << 13
            | (offen as u32) << 12
            | (offset as u32 & 0xFFF),
        0x80 << 24
            | (op as u32 >> 3) << 21
            | (srsrc as u32 >> 2) << 16
            | (vdata as u32) << 8
            | vaddr as u32,
    ]
}

const TBUFFER_LOAD_FORMAT_X: u8 = 0;
const TBUFFER_LOAD_FORMAT_XY: u8 = 1;
const TBUFFER_LOAD_FORMAT_XYZW: u8 = 3;
/// GFX10 buffer formats: 8 uint, 8_8_8_8 uint, 32_32 uint, 32_32_32_32 uint.
const FMT_8_UINT: u8 = 5;
const FMT_8888_UINT: u8 = 60;
const FMT_32_32_UINT: u8 = 62;
const FMT_32X4_UINT: u8 = 75;

/// Records of 5 dwords: the data the typed-format loads read.
fn records() -> Vec<u32> {
    (0..5 * 40)
        .map(|i| (0x0101_0101 * (i % 251)) ^ (i << 24))
        .collect()
}

/// 32 lanes read records of 20 bytes by index from the raw view at entry 1
/// (its entry's stride 20): four dwords at offset 4; four bytes at a lane's
/// offset of 0 or 4; one byte at offset 3.
fn indexed_format_program() -> Vec<u32> {
    let mut w = prologue();
    // v1 = t, v2 = 4 (t & 1), v14 = 64 t.
    w.push(vop1(V_MOV_B32, 1, v(0)));
    w.push(vop2(V_AND_B32, 2, int(1), 0));
    w.push(vop2(V_LSHLREV_B32, 2, int(2), 2));
    w.push(vop2(V_LSHLREV_B32, 14, int(6), 0));
    w.extend(mtbuf(
        TBUFFER_LOAD_FORMAT_XYZW,
        FMT_32X4_UINT,
        20,
        0,
        12,
        4,
        false,
        true,
    ));
    w.extend(mtbuf(
        TBUFFER_LOAD_FORMAT_XYZW,
        FMT_8888_UINT,
        24,
        1,
        12,
        0,
        true,
        true,
    ));
    w.extend(mtbuf(
        TBUFFER_LOAD_FORMAT_X,
        FMT_8_UINT,
        28,
        0,
        12,
        3,
        false,
        true,
    ));
    for k in 0..9u8 {
        w.extend(store_dword(20 + k, 14, 8, 4 * u16::from(k)));
    }
    w.push(S_ENDPGM);
    w
}

#[test]
fn typed_format_loads_by_index_from_a_table() {
    let shader = build_typed(&indexed_format_program(), 20, ResourceType::Srv).expect("lower");
    let data = records();
    let views = [
        View::RawUav(32 * 16),
        View::Buffer {
            data: &data,
            stride: Stride::Raw,
        },
    ];
    for (label, uavs) in run(&shader, &views, &[], &table_at_0) {
        for t in 0..32u32 {
            let r = 5 * t as usize;
            let bytes = data[r + (t & 1) as usize].to_le_bytes();
            let mut want: Vec<u32> = data[r + 1..r + 5].to_vec();
            want.extend(bytes.map(u32::from));
            want.push(data[r] >> 24);
            let at = (16 * t) as usize;
            assert_eq!(uavs[0][at..at + 9], want[..], "{label}: lane {t}");
        }
    }
    // Without a stride, by index is refused.
    let e = build_typed(&indexed_format_program(), 0, ResourceType::Srv).expect_err("refused");
    assert!(e.contains("unknown stride"), "{e}");
}

/// 32 lanes read two dwords at offset 4 of record t through a descriptor
/// built around the root SRV at s[1:2] with stride 12 (dword 3 `w3`).
fn built_format_program(w3: u32) -> Vec<u32> {
    let mut w = prologue();
    w.push(s_mov_b32(16, 1));
    // s_or_b32 s17, s2, 12 << 16
    w.extend([0x8000_0000 | 0x10 << 23 | 17 << 16 | 255 << 8 | 2, 12 << 16]);
    w.extend([s_mov_b32(18, 255), u32::MAX]);
    w.extend([s_mov_b32(19, 255), w3]);
    w.push(vop2(V_LSHLREV_B32, 14, int(3), 0));
    w.extend(mtbuf(
        TBUFFER_LOAD_FORMAT_XY,
        FMT_32_32_UINT,
        20,
        0,
        16,
        4,
        false,
        true,
    ));
    w.extend(store_dword(20, 14, 8, 0));
    w.extend(store_dword(21, 14, 8, 4));
    w.push(S_ENDPGM);
    w
}

fn build_built(words: &[u32]) -> Result<HostShader, String> {
    let root = RootParam {
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        sgprs: 2,
        base_register: 0,
        user_data: 1,
        slot: 1,
    };
    let srv = UsedResource {
        kind: ResourceType::Srv,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        // (The root SRV's entry says 1: the descriptor's own stride counts.)
        stride: 1,
        dwords: 4,
        user_data: 1,
        slot: 1,
    };
    build(
        words,
        32,
        0,
        vec![table(), root],
        vec![entry(ResourceType::Uav, 0, 0), srv],
    )
}

#[test]
fn typed_format_loads_by_index_through_a_built_descriptor() {
    let shader = build_built(&built_format_program(0x0002_7FAC)).expect("lower");
    let data = records();
    // The root SRV is the host's raw view of it (`emit`, "Root SRVs and
    // root UAVs"), at heap index 1, its address at byte 0 of the view.
    let views = [
        View::RawUav(64),
        View::Buffer {
            data: &data,
            stride: Stride::Raw,
        },
    ];
    let ud = |u: &UserData| match (u.root, u.dword) {
        (RootType::Table, _) => 0,
        (RootType::Descriptor, super::ROOT_SRV_VIEW) => 1,
        (RootType::Descriptor, super::ROOT_VIEW_OFFSET) => 0,
        other => panic!("unexpected user data {other:?}"),
    };
    for (label, uavs) in run(&shader, &views, &[], &ud) {
        for t in 0..32usize {
            let want = [data[3 * t + 1], data[3 * t + 2]];
            assert_eq!(uavs[0][2 * t..2 * t + 2], want, "{label}: lane {t}");
        }
    }
    // The lane index added to the index (dword 3 bit 23) is refused.
    let e = build_built(&built_format_program(0x0082_7FAC)).expect_err("refused");
    assert!(e.contains("swizzled"), "{e}");
}

/// User data: s0 the heap index of a buffer (a root constant), s1 the
/// resource heap, s2 a table with a raw UAV. Each lane reads two dwords at
/// offset 4 of element t of the heap's buffer by index, and stores them
/// to dwords 2t, 2t + 1 of the UAV.
fn heap_format_program() -> Vec<u32> {
    let mut w = vec![
        0x8F04_8500, // s_lshl_b32 s4, s0, 5
        s_mov_b32(6, 1),
        s_mov_b32(7, 128),
        0xF408_0203,
        0x0800_0000, // s_load_dwordx4 s[8:11], s[6:7], s4
        s_mov_b32(12, 2),
        s_mov_b32(13, 128),
    ];
    w.extend(s_load_dwordx4(16, 12, 0));
    w.push(vop2(V_LSHLREV_B32, 14, int(3), 0));
    w.extend(mtbuf(
        TBUFFER_LOAD_FORMAT_XY,
        FMT_32_32_UINT,
        20,
        0,
        8,
        4,
        false,
        true,
    ));
    w.extend(store_dword(20, 14, 16, 0));
    w.extend(store_dword(21, 14, 16, 4));
    w.push(S_ENDPGM);
    w
}

/// A typed-format load by index through a descriptor the shader takes from
/// the resource heap (no used-resource entry, so no stride) reads at
/// index × the view's stride, from the host's view-stride table.
#[test]
fn typed_format_loads_by_index_from_the_heap_take_the_views_stride() {
    let param = |root, user_data: u8| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot: user_data,
    };
    let uav = UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 2,
        slot: 2,
    };
    let shader = build(
        &heap_format_program(),
        32,
        0,
        vec![
            param(RootType::Constants, 0),
            param(RootType::ResourceHeap, 1),
            param(RootType::Table, 2),
        ],
        vec![uav],
    )
    .expect("lower");
    assert_eq!(shader.root_srvs, [super::VIEW_STRIDES]);
    let data = records();
    let views = [
        View::RawUav(64),
        View::Buffer {
            data: &data,
            stride: Stride::Raw,
        },
    ];
    // The stride table: heap index 1 holds 12-byte elements.
    let roots = [Root::View {
        srv: true,
        data: vec![4, 12],
    }];
    // The buffer is at heap index 1, the UAV's table at 0.
    let ud = |u: &UserData| match u.root {
        RootType::Constants => 1,
        RootType::Table => 0,
        other => panic!("unexpected user data {other:?}"),
    };
    for (label, uavs) in run(&shader, &views, &roots, &ud) {
        for t in 0..32usize {
            let want = [data[3 * t + 1], data[3 * t + 2]];
            assert_eq!(uavs[0][2 * t..2 * t + 2], want, "{label}: lane {t}");
        }
    }
}
