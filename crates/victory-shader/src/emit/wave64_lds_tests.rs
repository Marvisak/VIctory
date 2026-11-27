use super::*;
use crate::io;
use crate::ir;
use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::resource;
use crate::structure;

const S_ENDPGM: u32 = 0xBF81_0000;

fn lower_wave_cs(words: &[u32], threads: [u32; 3], lds_dwords: u32) -> Lowered {
    lower_wave_cs_layout(words, threads, lds_dwords, vec![])
}

fn lower_wave_cs_layout(
    words: &[u32],
    threads: [u32; 3],
    lds_dwords: u32,
    layout: Vec<crate::native::RootParam>,
) -> Lowered {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let lds_granules = lds_dwords.div_ceil(128).max(1);
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 256,
        layout,
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            // lds_size field in COMPUTE_PGM_RSRC2 is bits [15:23] in 128-dword granules.
            rsrc2: lds_granules << 15,
            threads,
        }),
        code,
    };
    let p = ir::build(&native.code, true, 256).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![],
        outputs: vec![],
        prims: vec![],
        mesh_stride: 0,
        view_marks: false,
    })
    .unwrap()
}

#[test]
fn wave64_lds_within_32_threads_drops_wave_size() {
    // Wave64 CS with <= 32 threads and uniform ds_write/ds_read fits in one Wave32 warp.
    let words = [
        0x7E00_0280, // v_mov_b32 v0, 0 (addr)
        0x7E02_0287, // v_mov_b32 v1, 7 (data)
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100, // v1 in src0
        0xD8D8_0000, // ds_read_b32 v2, v0
        0x0200_0000, // vdst = v2
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [32, 1, 1], 16);
    assert_eq!(lowered.wave_size, None);
    assert!(lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 3);
}

#[test]
fn wave64_lds_64_threads_uniform_drops_wave_size_and_emits_barriers() {
    // Wave64 CS with 64 threads and uniform ds_write/ds_read uses DXIL barriers instead of WaveSize(64).
    let words = [
        0x7E00_0280, // v_mov_b32 v0, 0 (addr)
        0x7E02_0287, // v_mov_b32 v1, 7 (data)
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100, // v1 in src0
        0xD8D8_0000, // ds_read_b32 v2, v0
        0x0200_0000, // vdst = v2
        S_ENDPGM,
    ];
    let mut lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, None);
    assert!(lowered.lds_wave32_sync);
    // 1 barrier after ds_write_b32 + 2 barriers (before & after) around ds_read_b32.
    assert_eq!(lowered.lds_barriers, 3);
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        match v.validate(&mut lowered.dxil) {
            Ok(()) => {}
            Err(e) if e.contains("Unknown shader model") => {}
            Err(e) => panic!("DXIL validation failed: {e}"),
        }
    }
}

#[test]
fn wave64_lds_64_threads_guarded_keeps_wave_size_64() {
    // Divergent EXEC-guarded LDS access cannot safely insert a group barrier; keep WaveSize(64).
    let words = [
        0x7E02_0287, // v_mov_b32 v1, 7
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0 (v0 = local thread id x)
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_divergent_branch_keeps_wave_size_64() {
    // Divergent s_cbranch_execz around an LDS write leaves guard=None inside the block,
    // so control-flow reachability must still keep WaveSize(64) and emit zero barriers.
    let words = [
        0x7E02_0287, // v_mov_b32 v1, 7
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xBF88_0002, // s_cbranch_execz +2 dwords (skip ds_write_b32)
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_readfirstlane_keeps_wave_size_64() {
    // Non-waterfall v_readfirstlane_b32 changes meaning when split across two Wave32 waves.
    let words = [
        0x7E00_0500, // v_readfirstlane_b32 s0, v0
        0x7E02_0200, // v_mov_b32 v1, s0
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_scalar_mask_mod_keeps_wave_size_64() {
    // Modifying a lane mask with s_bitset1_b32 and branching on it can diverge across two Wave32s.
    let words = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_046A, // s_mov_b64 s[20:21], vcc
        0xBE94_1D80, // s_bitset1_b32 s20, 0
        0xBF06_8114, // s_cmp_eq_u32 s20, 1
        0xBF85_0002, // s_cbranch_scc1 +2
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_high_mask_read_keeps_wave_size_64() {
    // Reading the upper dword (x.st.highs) of a 64-bit lane mask must not allow Wave32 LDS sync.
    let words = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_046A, // s_mov_b64 s[20:21], vcc
        0xBE95_1D80, // s_bitset1_b32 s21, 0
        0xBF06_8115, // s_cmp_eq_u32 s21, 1
        0xBF85_0002, // s_cbranch_scc1 +2
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_cndmask_data_keeps_wave_size_64() {
    // Passing a tracked lane mask as a data operand to v_cndmask_b32 materializes a Wave32 ballot.
    let words = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_046A, // s_mov_b64 s[20:21], vcc
        0x0202_0214, // v_cndmask_b32 v1, s20, v1, vcc
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs(&words, [64, 1, 1], 16);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}

#[test]
fn wave64_lds_64_threads_smem_mask_offset_keeps_wave_size_64() {
    // Using a tracked lane mask SGPR as an SMEM soffset materializes a Wave32 ballot.
    let layout = vec![crate::native::RootParam {
        root: crate::native::RootType::Constants,
        table_offset_dwords: 0,
        sgprs: 4,
        base_register: 0,
        user_data: 0,
        slot: 0,
    }];
    let words = [
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_046A, // s_mov_b64 s[20:21], vcc
        0xF400_0100, // s_load_dword s4, s[0:1], s20
        0x2800_0000,
        0x7E02_0204, // v_mov_b32 v1, s4
        0xD834_0000, // ds_write_b32 v0, v1
        0x0000_0100,
        S_ENDPGM,
    ];
    let lowered = lower_wave_cs_layout(&words, [64, 1, 1], 16, layout);
    assert_eq!(lowered.wave_size, Some(64));
    assert!(!lowered.lds_wave32_sync);
    assert_eq!(lowered.lds_barriers, 0);
}
