//! End-to-end tests of synthetic GFX10 LDS atomic shaders lowered to DXIL,
//! validated by `dxil.dll` when present, and executed on the host GPU and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const S_BARRIER: u32 = 0xBF8A_0000;

const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

const fn s_load_dwordx4(dst: u8, base: u8) -> [u32; 2] {
    [
        0x3D << 26 | 0x02 << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25,
    ]
}

const fn vop1(op: u16, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | (op as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn vop2(op: u8, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    (op as u32 & 0x3F) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn vopc(op: u8, src0: u16, vsrc1: u8) -> u32 {
    0x7C00_0000 | (op as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
}

const fn ds(op: u8, vdst: u8, addr: u8, data0: u8, data1: u8, off0: u8, off1: u8) -> [u32; 2] {
    [
        0x36 << 26 | (op as u32) << 18 | (off1 as u32) << 8 | off0 as u32,
        (vdst as u32) << 24 | (data1 as u32) << 16 | (data0 as u32) << 8 | addr as u32,
    ]
}

const fn ds_16(op: u8, vdst: u8, addr: u8, data0: u8, data1: u8, offset: u16) -> [u32; 2] {
    ds(
        op,
        vdst,
        addr,
        data0,
        data1,
        offset as u8,
        (offset >> 8) as u8,
    )
}

const fn buffer_store_dword(vdata: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8,
    ]
}

const fn buffer_store_dword_offen(vdata: u8, vaddr: u8, srsrc: u8, byte_off: u16) -> [u32; 2] {
    [
        0x38 << 26 | 0x1C << 18 | 1 << 12 | (byte_off as u32 & 0xFFF),
        0x80 << 24 | ((srsrc as u32 >> 2) & 0x1F) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

const fn sgpr(n: u8) -> u16 {
    n as u16
}

/// Lowers and validates a synthetic compute program with 4 root-constant dwords
/// (`s0..s3`), a descriptor table at `s4` (raw UAV at heap index 0), and 1 KiB
/// of LDS (`2 << 15` in `rsrc2`).
fn build_ds_cs(words: &[u32], threads: u32) -> HostShader {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 15 | 5 << 1,
            threads: [threads, 1, 1],
        }),
        code,
    };

    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
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
    })
    .expect("lower");

    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
    };

    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        let ours = copy[4..20].to_vec();
        v.validate(&mut copy).expect("DXIL validation failed");
        assert_eq!(copy[4..20], ours[..], "container hash mismatch");
    }

    shader
}

fn run_uav(
    shader: &HostShader,
    uav_dwords: u32,
    cases: &[[u32; 4]],
    mut check: impl FnMut(&str, usize, [u32; 4], &[u32]),
) {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        for (i, &inputs) in cases.iter().enumerate() {
            let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => inputs[u.dword as usize],
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &[View::RawUav(uav_dwords)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            check(label, i, inputs, &out[0]);
        }
        ran += 1;
    }
    assert!(ran > 0, "neither hardware nor WARP SM 6.6 device available");
}

/// Builds a 64-thread shader (`s.wave_size = None` so both hardware and WARP run
/// it) where lane 0 branches into an unguarded block, initializes LDS at `off_a`
/// from `s0` (`v1`) and `off_b` from `s3` (`v4`), copies `s1, s2` into `v2, v3`,
/// presets `v6 = 0xDEAD_BEEF` and `v7 = 0xCAFE_BABE`, runs `atomic`, reads LDS at
/// `off_a` and `off_b` into `v8, v9`, and writes `[v8, v6, v9, v7]`
/// (`[new0, old0, new1, old1]`) to the UAV.
fn build_atomic_probe(off_a: u16, off_b: u16, atomic: [u32; 2]) -> HostShader {
    let mut body = vec![vop1(0x001, 0, 0x80)]; // v0 = 0 (LDS base)
    for i in 0..4u8 {
        body.push(vop1(0x001, i + 1, sgpr(i))); // v1..v4 = s0..s3
    }
    body.extend_from_slice(&[vop1(0x001, 6, 0xFF), 0xDEAD_BEEF]); // v6 = sentinel
    body.extend_from_slice(&[vop1(0x001, 7, 0xFF), 0xCAFE_BABE]); // v7 = sentinel
    body.extend_from_slice(&ds_16(0x0D, 0, 0, 1, 0, off_a)); // ds_write_b32 v0, v1 offset:off_a
    body.extend_from_slice(&ds_16(0x0D, 0, 0, 4, 0, off_b)); // ds_write_b32 v0, v4 offset:off_b
    body.extend_from_slice(&atomic);
    body.extend_from_slice(&ds_16(0x36, 8, 0, 0, 0, off_a)); // ds_read_b32 v8, v0 offset:off_a
    body.extend_from_slice(&ds_16(0x36, 9, 0, 0, 0, off_b)); // ds_read_b32 v9, v0 offset:off_b
    body.extend_from_slice(&buffer_store_dword(8, 8, 0));
    body.extend_from_slice(&buffer_store_dword(6, 8, 4));
    body.extend_from_slice(&buffer_store_dword(9, 8, 8));
    body.extend_from_slice(&buffer_store_dword(7, 8, 12));

    let mut w = vec![s_mov_b32(5, 0x80)]; // s5 = 0
    w.extend_from_slice(&s_load_dwordx4(8, 4));
    w.push(vopc(0xC2, 0x80, 0)); // v_cmp_eq_u32 vcc_lo, 0, v0
    w.push(0xBE86_3C6A); // s_and_saveexec_b32 s6, vcc_lo
    w.push(0xBF88_0000 | (body.len() as u32 & 0xFFFF)); // s_cbranch_execz -> restore
    w.extend_from_slice(&body);
    w.push(0x887E_067E); // restore: s_or_b32 exec_lo, exec_lo, s6
    w.push(S_ENDPGM);
    build_ds_cs(&w, 64)
}

#[test]
fn ds_rsub_inc_dec_mskor_wrap_and_wrxchg2_match_rdna2() {
    let sentinel = 0xDEAD_BEEFu32;

    // ds_rsub_u32 (0x02) and ds_rsub_rtn_u32 (0x22): mem = data0 - mem.
    let rsub_cases: &[([u32; 2], u32)] = &[
        ([37, 100], 63),
        ([10, 5], 5u32.wrapping_sub(10)),
        ([1, 0], u32::MAX),
        ([0, 0], 0),
        ([u32::MAX, u32::MAX], 0),
    ];
    for (op, rtn) in [(0x02u8, false), (0x22, true)] {
        let shader = build_atomic_probe(16, 32, ds_16(op, 6, 0, 2, 0, 16));
        let inputs: Vec<[u32; 4]> = rsub_cases
            .iter()
            .map(|&(c, _)| [c[0], c[1], 0, 0])
            .collect();
        run_uav(&shader, 4, &inputs, |backend, i, inp, out| {
            let want_new = rsub_cases[i].1;
            let want_old = if rtn { inp[0] } else { sentinel };
            assert_eq!(
                (out[0], out[1]),
                (want_new, want_old),
                "{backend}: ds_rsub(rtn={rtn}) mem={:#x} d0={:#x}",
                inp[0],
                inp[1],
            );
        });
    }

    // ds_inc_u32 (0x03) and ds_inc_rtn_u32 (0x23): (mem >= data0) ? 0 : mem + 1 (unsigned).
    let inc_cases: &[([u32; 2], u32)] = &[
        ([0, 5], 1),
        ([4, 5], 5),
        ([5, 5], 0),
        ([9, 5], 0),
        ([0, 0], 0),
        ([u32::MAX - 1, u32::MAX], u32::MAX),
        ([u32::MAX, u32::MAX], 0),
        ([0x8000_0000, 0x7FFF_FFFF], 0), // unsigned >=, not signed
        ([0x7FFF_FFFF, 0x8000_0000], 0x8000_0000),
    ];
    for (op, rtn) in [(0x03u8, false), (0x23, true)] {
        let shader = build_atomic_probe(16, 32, ds_16(op, 6, 0, 2, 0, 16));
        let inputs: Vec<[u32; 4]> = inc_cases.iter().map(|&(c, _)| [c[0], c[1], 0, 0]).collect();
        run_uav(&shader, 4, &inputs, |backend, i, inp, out| {
            let want_new = inc_cases[i].1;
            let want_old = if rtn { inp[0] } else { sentinel };
            assert_eq!(
                (out[0], out[1]),
                (want_new, want_old),
                "{backend}: ds_inc(rtn={rtn}) mem={:#x} d0={:#x}",
                inp[0],
                inp[1],
            );
        });
    }

    // ds_dec_u32 (0x04) and ds_dec_rtn_u32 (0x24): (mem == 0 || mem > data0) ? data0 : mem - 1.
    let dec_cases: &[([u32; 2], u32)] = &[
        ([0, 5], 5),
        ([1, 5], 0),
        ([5, 5], 4),
        ([6, 5], 5),
        ([0, 0], 0),
        ([1, 0], 0),
        ([0, u32::MAX], u32::MAX),
        ([u32::MAX, u32::MAX], u32::MAX - 1),
        ([0x8000_0000, 0x7FFF_FFFF], 0x7FFF_FFFF), // unsigned >, not signed
        ([0x8000_0000, 0x8000_0000], 0x7FFF_FFFF),
    ];
    for (op, rtn) in [(0x04u8, false), (0x24, true)] {
        let shader = build_atomic_probe(16, 32, ds_16(op, 6, 0, 2, 0, 16));
        let inputs: Vec<[u32; 4]> = dec_cases.iter().map(|&(c, _)| [c[0], c[1], 0, 0]).collect();
        run_uav(&shader, 4, &inputs, |backend, i, inp, out| {
            let want_new = dec_cases[i].1;
            let want_old = if rtn { inp[0] } else { sentinel };
            assert_eq!(
                (out[0], out[1]),
                (want_new, want_old),
                "{backend}: ds_dec(rtn={rtn}) mem={:#x} d0={:#x}",
                inp[0],
                inp[1],
            );
        });
    }

    // ds_mskor_b32 (0x0c) and ds_mskor_rtn_b32 (0x2c): (mem & ~data0) | data1.
    let mskor_cases: &[([u32; 3], u32)] = &[
        ([0xAABB_CCDD, 0x00FF_00FF, 0x0012_0034], 0xAA12_CC34),
        ([0x1234_5678, 0x0000_0000, 0x8000_0001], 0x9234_5679),
        ([0xFFFF_FFFF, 0xFFFF_FFFF, 0x0102_0304], 0x0102_0304),
        // data1 bits outside data0 are still ORed in per RDNA2 ISA.
        ([0xF000_000F, 0x0000_00FF, 0x0100_005A], 0xF100_005A),
    ];
    for (op, rtn) in [(0x0Cu8, false), (0x2C, true)] {
        let shader = build_atomic_probe(16, 32, ds_16(op, 6, 0, 2, 3, 16));
        let inputs: Vec<[u32; 4]> = mskor_cases
            .iter()
            .map(|&(c, _)| [c[0], c[1], c[2], 0])
            .collect();
        run_uav(&shader, 4, &inputs, |backend, i, inp, out| {
            let want_new = mskor_cases[i].1;
            let want_old = if rtn { inp[0] } else { sentinel };
            assert_eq!(
                (out[0], out[1]),
                (want_new, want_old),
                "{backend}: ds_mskor(rtn={rtn}) {inp:#x?}",
            );
        });
    }

    // ds_wrap_rtn_b32 (0x34): (mem >= data0) ? mem - data0 : mem + data1 (unsigned).
    let wrap_cases: &[([u32; 3], u32)] = &[
        ([7, 10, 3], 10),
        ([10, 10, 3], 0),
        ([14, 10, 3], 4),
        ([5, 0, 99], 5), // mem >= 0 always holds unsigned
        ([0x8000_0005, 0x8000_0000, 9], 5),
        ([0x7FFF_FFFE, 0x8000_0000, 5], 0x8000_0003),
    ];
    let wrap_shader = build_atomic_probe(16, 32, ds_16(0x34, 6, 0, 2, 3, 16));
    let wrap_inputs: Vec<[u32; 4]> = wrap_cases
        .iter()
        .map(|&(c, _)| [c[0], c[1], c[2], 0])
        .collect();
    run_uav(&wrap_shader, 4, &wrap_inputs, |backend, i, inp, out| {
        assert_eq!(
            (out[0], out[1]),
            (wrap_cases[i].1, inp[0]),
            "{backend}: ds_wrap_rtn_b32 {inp:#x?}",
        );
    });

    // ds_wrxchg2_rtn_b32 (0x2e, scale 4) and ds_wrxchg2st64_rtn_b32 (0x2f, scale 256).
    for (op, off_a, off_b) in [(0x2Eu8, 4u16, 12u16), (0x2F, 256, 768)] {
        let shader = build_atomic_probe(off_a, off_b, ds(op, 6, 0, 2, 3, 1, 3));
        let cases = [
            [0x1111_2222, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x3333_4444],
            [0, u32::MAX, 1, 0x8000_0000],
        ];
        run_uav(&shader, 4, &cases, |backend, _, inp, out| {
            assert_eq!(
                out,
                &[inp[1], inp[0], inp[2], inp[3]],
                "{backend}: wrxchg2 op={op:#x} {inp:#x?}",
            );
        });
    }
}

#[test]
fn ds_atomics_handle_wave_contention_and_divergent_exec_guards() {
    // 64 threads (v0 = thread id 0..63):
    // 1. Thread 0 writes LDS[0] = 0, LDS[4] = 20; all threads sync at s_barrier.
    // 2. All 64 threads contend on LDS[0] with `ds_inc_rtn_u32 v5, v_zero, v_nine`
    //    (limit = 9 -> 64 increments wrap 6 times and leave LDS[0] = 4).
    // 3. Narrow EXEC to odd threads (`(v0 & 1) == 1`) with `v_cmpx_eq_u32` (no branch,
    //    so Lower::ds runs under a lane guard) and execute:
    //      - `ds_dec_rtn_u32 v6, v_zero, v_hundred offset:4` (preset v6 = 0x7777_0000 + tid)
    //      - `ds_wrxchg2_rtn_b32 v[7:8], v_slot, v_a, v_b offset0:0 offset1:1`
    //    Restore EXEC = -1, sync at s_barrier, and write per-lane results + final LDS[0..1].
    let mut w = vec![s_mov_b32(5, 0x80)]; // s5 = 0
    w.extend_from_slice(&s_load_dwordx4(8, 4));
    w.push(vop1(0x001, 1, 0x80)); // v1 = 0 (LDS address 0)
    w.push(vop1(0x001, 2, 0x80 + 20)); // v2 = 20

    // Guarded init by thread 0: v_cmpx_eq_u32 0, v0; ds_write_b32 LDS[0]=0, LDS[4]=20; exec=-1.
    w.push(vopc(0x92, 0x80, 0)); // v_cmpx_eq_u32 0, v0 (VOPC 0x92)
    w.extend_from_slice(&ds_16(0x0D, 0, 1, 1, 0, 0));
    w.extend_from_slice(&ds_16(0x0D, 0, 1, 2, 0, 4));
    // Also initialize per-lane pair slots at byte 16 + 8 * tid to (0x1000 + tid, 0x2000 + tid).
    w.push(s_mov_b32(126, 0xC1)); // s_mov_b32 exec_lo, -1
    w.push(vop2(0x1A, 3, 0x83, 0)); // v_lshlrev_b32 v3, 3, v0 (8 * tid)
    w.extend_from_slice(&[vop2(0x25, 10, 0xFF, 0), 0x1000]); // v_add_nc_u32 v10, 0x1000, v0
    w.extend_from_slice(&[vop2(0x25, 11, 0xFF, 0), 0x2000]); // v_add_nc_u32 v11, 0x2000, v0
    w.extend_from_slice(&ds_16(0x0D, 0, 3, 10, 0, 16));
    w.extend_from_slice(&ds_16(0x0D, 0, 3, 11, 0, 20));
    w.push(S_BARRIER);

    // Contended across all 64 lanes: ds_inc_rtn_u32 v5, v1, v4 (v4 = 9).
    w.push(vop1(0x001, 4, 0x80 + 9)); // v4 = 9
    w.extend_from_slice(&ds_16(0x23, 5, 1, 4, 0, 0));

    // Presets before narrowing EXEC so disabled (even) lanes must preserve them.
    w.extend_from_slice(&[vop2(0x25, 6, 0xFF, 0), 0x7777_0000]); // v6 = 0x7777_0000 + tid
    w.extend_from_slice(&[vop2(0x25, 7, 0xFF, 0), 0x8888_0000]); // v7 = 0x8888_0000 + tid
    w.extend_from_slice(&[vop2(0x25, 8, 0xFF, 0), 0x9999_0000]); // v8 = 0x9999_0000 + tid
    w.extend_from_slice(&[vop1(0x001, 9, 0xFF), 100]); // v9 = 100
    w.extend_from_slice(&[vop2(0x25, 12, 0xFF, 0), 0x3000]); // v12 = 0x3000 + tid
    w.extend_from_slice(&[vop2(0x25, 13, 0xFF, 0), 0x4000]); // v13 = 0x4000 + tid

    // Odd lanes only: v14 = v0 & 1; v_cmpx_eq_u32 1, v14.
    w.push(vop2(0x1B, 14, 0x81, 0)); // v_and_b32 v14, 1, v0
    w.push(vopc(0x92, 0x81, 14)); // v_cmpx_eq_u32 1, v14
    w.extend_from_slice(&ds_16(0x24, 6, 1, 9, 0, 4)); // ds_dec_rtn_u32 v6, v1, v9 offset:4
    w.extend_from_slice(&ds(0x2E, 7, 3, 12, 13, 4, 5)); // ds_wrxchg2_rtn_b32 v[7:8], v3, v12, v13 offset0:4 offset1:5
    w.push(s_mov_b32(126, 0xC1)); // s_mov_b32 exec_lo, -1
    w.push(S_BARRIER);

    // Store per-lane [v5, v6, v7, v8, new_slot0, new_slot1] (6 dwords = 24 bytes per thread),
    // and final [LDS[0], LDS[4]] at dwords 384..386.
    w.extend_from_slice(&ds_16(0x36, 10, 3, 0, 0, 16));
    w.extend_from_slice(&ds_16(0x36, 11, 3, 0, 0, 20));
    w.extend_from_slice(&[vop2(0x0B, 15, 0xFF, 0), 24]); // v_mul_u32_u24 v15, 24, v0
    for (k, reg) in [5u8, 6, 7, 8, 10, 11].into_iter().enumerate() {
        w.extend_from_slice(&buffer_store_dword_offen(reg, 15, 8, (4 * k) as u16));
    }
    w.extend_from_slice(&ds_16(0x36, 12, 1, 0, 0, 0));
    w.extend_from_slice(&ds_16(0x36, 13, 1, 0, 0, 4));
    w.extend_from_slice(&buffer_store_dword(12, 8, 384 * 4));
    w.extend_from_slice(&buffer_store_dword(13, 8, 385 * 4));
    w.push(S_ENDPGM);

    let shader = build_ds_cs(&w, 64);
    run_uav(&shader, 386, &[[0; 4]], |backend, _, _, out| {
        // Final LDS[0]: 64 increments mod 10 = 4.
        assert_eq!(out[384], 4, "{backend}: final LDS[0]");
        // Final LDS[4]: 32 odd-lane decrements from 20 with limit 100 -> 89.
        assert_eq!(out[385], 89, "{backend}: final LDS[4]");

        let mut inc_hist = [0u32; 10];
        let mut odd_decs = Vec::new();
        for tid in 0..64usize {
            let base = 6 * tid;
            let (inc_old, dec_old, x0, x1, s0, s1) = (
                out[base],
                out[base + 1],
                out[base + 2],
                out[base + 3],
                out[base + 4],
                out[base + 5],
            );
            assert!(inc_old < 10, "{backend}: tid {tid} inc_old={inc_old}");
            inc_hist[inc_old as usize] += 1;

            let tid_u = tid as u32;
            if tid % 2 == 0 {
                // Disabled lanes must neither touch LDS nor clobber v6..v8.
                assert_eq!(dec_old, 0x7777_0000 + tid_u, "{backend}: even tid {tid} v6");
                assert_eq!(x0, 0x8888_0000 + tid_u, "{backend}: even tid {tid} v7");
                assert_eq!(x1, 0x9999_0000 + tid_u, "{backend}: even tid {tid} v8");
                assert_eq!(
                    (s0, s1),
                    (0x1000 + tid_u, 0x2000 + tid_u),
                    "{backend}: even tid {tid} LDS untouched",
                );
            } else {
                odd_decs.push(dec_old);
                assert_eq!(
                    (x0, x1),
                    (0x1000 + tid_u, 0x2000 + tid_u),
                    "{backend}: odd tid {tid} wrxchg2 old",
                );
                assert_eq!(
                    (s0, s1),
                    (0x3000 + tid_u, 0x4000 + tid_u),
                    "{backend}: odd tid {tid} wrxchg2 new",
                );
            }
        }
        assert_eq!(
            inc_hist,
            [7, 7, 7, 7, 6, 6, 6, 6, 6, 6],
            "{backend}: contended ds_inc_rtn_u32 histogram",
        );
        odd_decs.sort_unstable();
        let mut want_decs: Vec<u32> = (0..=20).chain(90..=100).collect();
        want_decs.sort_unstable();
        assert_eq!(
            odd_decs, want_decs,
            "{backend}: contended guarded ds_dec_rtn_u32 old values"
        );
    });
}

#[test]
fn ds_cas_loops_nest_inside_shader_loops() {
    // A shader loop running 5 iterations in lane 0, with back-to-back CAS-loop
    // atomics (`ds_wrap_rtn_b32` and `ds_mskor_rtn_b32`) in the loop body so
    // `Lower`'s loop-counter phis, `tail[b]`, and DXIL natural-loop nesting
    // are exercised together.
    let mut body = vec![
        vop1(0x001, 0, 0x80),  // v0 = 0 (LDS base)
        vop1(0x001, 1, 0x82),  // v1 = 2 (initial wrap value)
        vop1(0x001, 2, 0x87),  // v2 = 7 (wrap limit)
        vop1(0x001, 3, 0x84),  // v3 = 4 (wrap step)
        vop1(0x001, 6, 0x80),  // v6 = 0 (sum of old wrap values)
        vop1(0x001, 10, 0x8F), // v10 = 0xF
    ];
    body.extend_from_slice(&ds_16(0x0D, 0, 0, 1, 0, 0)); // LDS[0] = 2
    body.extend_from_slice(&[vop1(0x001, 11, 0xFF), 0xABC0_0000]);
    body.extend_from_slice(&ds_16(0x0D, 0, 0, 11, 0, 4)); // LDS[4] = 0xABC0_0000
    body.push(s_mov_b32(7, 0x80)); // s7 = 0 (loop counter)

    let mut iter = vec![vop1(0x001, 4, sgpr(7))]; // v4 = s7
    iter.extend_from_slice(&ds_16(0x34, 5, 0, 2, 3, 0)); // ds_wrap_rtn_b32 v5, v0, v2, v3
    iter.push(vop2(0x25, 6, 256 + 5, 6)); // v6 += v5
    iter.push(vop2(0x1A, 7, 0x82, 4)); // v7 = v4 << 2
    iter.push(vop2(0x1A, 8, 256 + 7, 10)); // v8 = 0xF << v7 (mask)
    iter.push(vop2(0x25, 9, 0x81, 4)); // v9 = v4 + 1
    iter.push(vop2(0x1A, 9, 256 + 7, 9)); // v9 = (v4 + 1) << v7 (bits)
    iter.extend_from_slice(&ds_16(0x2C, 12, 0, 8, 9, 4)); // ds_mskor_rtn_b32 v12, v0, v8, v9 offset:4
    iter.push(0x8007_8107); // s_add_u32 s7, s7, 1
    iter.push(0xBF0A_8507); // s_cmp_lt_u32 s7, 5
    let back = -((iter.len() as i32) + 1) as i16;
    iter.push(0xBF85_0000 | (back as u16 as u32)); // s_cbranch_scc1 -> iter start

    body.extend_from_slice(&iter);
    body.extend_from_slice(&ds_16(0x36, 13, 0, 0, 0, 0)); // v13 = final LDS[0]
    body.extend_from_slice(&ds_16(0x36, 14, 0, 0, 0, 4)); // v14 = final LDS[4]
    body.extend_from_slice(&buffer_store_dword(13, 8, 0));
    body.extend_from_slice(&buffer_store_dword(6, 8, 4));
    body.extend_from_slice(&buffer_store_dword(14, 8, 8));

    let mut w = vec![s_mov_b32(5, 0x80)];
    w.extend_from_slice(&s_load_dwordx4(8, 4));
    w.push(vopc(0xC2, 0x80, 0)); // v_cmp_eq_u32 vcc_lo, 0, v0
    w.push(0xBE86_3C6A); // s_and_saveexec_b32 s6, vcc_lo
    w.push(0xBF88_0000 | (body.len() as u32 & 0xFFFF));
    w.extend_from_slice(&body);
    w.push(0x887E_067E); // s_or_b32 exec_lo, exec_lo, s6
    w.push(S_ENDPGM);

    // Wrap sequence from 2 with (limit=7, step=4) over 5 steps:
    // 2 -> 6 -> 10 -> 3 -> 7 -> 0 (old values summed: 2 + 6 + 10 + 3 + 7 = 28, final = 0).
    // MskOr sequence on 0xABC0_0000 over iter 0..5 writes nibbles 1,2,3,4,5 -> 0xABC5_4321.
    let shader = build_ds_cs(&w, 64);
    run_uav(&shader, 4, &[[0; 4]], |backend, _, _, out| {
        assert_eq!(
            &out[..3],
            &[0, 28, 0xABC5_4321],
            "{backend}: nested CAS loops in shader loop",
        );
    });
}
