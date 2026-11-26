use super::super::container::SigElement;
use super::{Lowered, Parts, lower};
use crate::native::{HwStage, NativeProgram, PsInput, StageIo, Usage};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

fn lower_ps(words: &[u32], flat: bool, fp16: bool) -> Lowered {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![PsInput {
                usage: Usage::Generic,
                row: 0,
                mask: 0xF,
                attr: 0,
                flat,
                fp16,
                pack2: false,
                row2: 0,
            }],
            // Perspective center barycentrics (v0, v1) + SV_Position.x (v2).
            input_addr: 1 << 1 | 1 << 8,
            db_shader_control: 0,
            patches: vec![],
        },
        code,
    };
    let el = |name: &str, kind, row, usage, interp, half| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp,
        row: Some(row),
        col: 0,
        cols: 4,
        usage,
        half,
    };
    let p = ir::build(&native.code, false, 8).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    lower(&Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: vec![
            el("TEXCOORD", 0, 0, 0, 0, fp16),
            el("SV_Position", 3, 1, 0, 4, false),
        ],
        outputs: vec![el("SV_Target", 16, 0, 15, 0, false)],
        prims: vec![],
        mesh_stride: 0,
        view_marks: false,
    })
    .unwrap()
}

#[test]
fn guarded_interp_preserves_inactive_lanes_prior_vgpr_value() {
    const SENTINEL: u32 = 0x1234_5678;

    // 1) Guarded `v_interp_mov_f32 v3, p0, attr0.x`: inactive lanes must keep SENTINEL.
    let mov_words = [
        0xBE80_037E, // s_mov_b32 s0, exec_lo
        0x7E06_02FF,
        SENTINEL,    // v_mov_b32 v3, SENTINEL
        0x7C22_0480, // v_cmpx_lt_f32 exec, 0, v2
        0xC80E_0002, // v_interp_mov_f32 v3, p0, attr0.x
        0xBEFE_0300, // s_mov_b32 exec_lo, s0
        0xF800_180F,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
        S_ENDPGM,
    ];
    let mov_l = lower_ps(&mov_words, true, false);
    assert_eq!(mov_l.select_false_ints, vec![Some(i64::from(SENTINEL as i32))]);
    #[cfg(windows)]
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut mov_l.dxil.clone())
            .expect("guarded v_interp_mov_f32 DXIL validation failed");
    }

    // 2) Guarded `v_interp_p1_f32` + `v_interp_p2_f32` into the same VGPR v3:
    //    inactive lanes must still keep SENTINEL (not 0 and not overwritten).
    let pair_words = [
        0xBE80_037E, // s_mov_b32 s0, exec_lo
        0x7E06_02FF,
        SENTINEL,    // v_mov_b32 v3, SENTINEL
        0x7C22_0480, // v_cmpx_lt_f32 exec, 0, v2
        0xC80C_0000, // v_interp_p1_f32 v3, v0, attr0.x
        0xC80D_0001, // v_interp_p2_f32 v3, v1, attr0.x
        0xBEFE_0300, // s_mov_b32 exec_lo, s0
        0xF800_180F,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
        S_ENDPGM,
    ];
    let pair_l = lower_ps(&pair_words, false, false);
    assert_eq!(pair_l.select_false_ints, vec![Some(i64::from(SENTINEL as i32))]);
    #[cfg(windows)]
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut pair_l.dxil.clone())
            .expect("guarded v_interp_p2_f32 DXIL validation failed");
    }

    // 3) Guarded `v_interp_p1ll_f16` + `v_interp_p2_f16` sharing destination
    //    VGPR v3: inactive lanes must preserve both halves of pre-p1 v3.
    const SENTINEL_F16: u32 = 0x4000_3C00; // (1.0f16, 2.0f16)
    let f16_pair_words = [
        0xBE80_037E, // s_mov_b32 s0, exec_lo
        0x7E06_02FF,
        SENTINEL_F16, // v_mov_b32 v3, 0x4000_3C00
        0x7C22_0480,  // v_cmpx_lt_f32 exec, 0, v2
        0xD742_0003,
        0x0002_0000, // v_interp_p1ll_f16 v3, v0, attr0.x
        0xD75A_0003,
        0x040E_0200, // v_interp_p2_f16 v3, v1, attr0.x, v3
        0xBEFE_0300, // s_mov_b32 exec_lo, s0
        0xF800_180F,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
        S_ENDPGM,
    ];
    let f16_pair_l = lower_ps(&f16_pair_words, false, true);
    let f16_const_false: Vec<i64> = f16_pair_l
        .select_false_ints
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(f16_const_false, vec![i64::from(SENTINEL_F16 as i32)]);
    #[cfg(windows)]
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut f16_pair_l.dxil.clone())
            .expect("guarded shared-dest v_interp_p2_f16 DXIL validation failed");
    }

    // 4) Unguarded `v_interp_mov_f32` emits no select.
    let unguarded = [
        0xC80E_0002, // v_interp_mov_f32 v3, p0, attr0.x
        0xF800_180F,
        0x0303_0303, // exp mrt0, v3, v3, v3, v3 done vm
        S_ENDPGM,
    ];
    assert!(lower_ps(&unguarded, true, false).select_false_ints.is_empty());
}
