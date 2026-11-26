use super::{Lowered, Parts, lower};
use crate::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

fn lower_compute(words: &[u32]) -> Lowered {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15 | 2 << 1,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, false, 256).unwrap();
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
fn wave_prefix_op_sets_signed_op_kind_for_float_and_unsigned_for_int() {
    const fn sop1(sdst: u8, op: u8, ssrc0: u8) -> u32 {
        0xBE80_0000 | (sdst as u32) << 16 | (op as u32) << 8 | ssrc0 as u32
    }
    const fn vop2(vdst: u8, op: u8, src0: u16, vsrc1: u8) -> u32 {
        (op as u32) << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | (src0 as u32 & 0x1FF)
    }
    const fn dpp16(vsrc0: u8, ctrl: u16, row_mask: u8, bank_mask: u8, bound_ctrl: bool) -> u32 {
        (row_mask as u32) << 28
            | (bank_mask as u32) << 24
            | (bound_ctrl as u32) << 19
            | (ctrl as u32) << 8
            | vsrc0 as u32
    }
    const fn vop3(vdst: u8, op: u16, src0: u16, src1: u16, src2: u16) -> [u32; 2] {
        [
            0xD400_0000 | (op as u32) << 16 | vdst as u32,
            (src0 as u32 & 0x1FF)
                | ((src1 as u32 & 0x1FF) << 9)
                | ((src2 as u32 & 0x1FF) << 18),
        ]
    }
    let prefix_program = |add_op: u8| {
        let mut w = Vec::new();
        // Wave32 inclusive prefix sum over v0 into v1, rooted by ds_write_b32.
        w.push(sop1(0, 0x3D, 193)); // s_or_saveexec_b32 s0, -1
        w.extend(vop3(1, 0x101, 128, 256, 0)); // v_cndmask_b32_e64 v1, 0, v0, s0
        for shift in [1u16, 2, 4, 8] {
            w.push(vop2(1, add_op, 0xFA, 1));
            w.push(dpp16(1, 0x110 + shift, 0xF, 0xF, false));
        }
        w.extend(vop3(2, 0x360, 257, 128 + 15, 0)); // v_readlane_b32 s2, v1, 15
        w.push(sop1(crate::ir::EXEC, 0x03, 255)); // s_mov_b32 exec_lo, 0xFFFF_0000
        w.push(0xFFFF_0000);
        w.push(vop2(1, add_op, 2, 1)); // v_add_{f32,nc_u32} v1, s2, v1
        w.push(sop1(crate::ir::EXEC, 0x03, 0)); // s_mov_b32 exec_lo, s0
        w.push(0xD834_0000);
        w.push(0x0000_0100); // ds_write_b32 v0, v1
        w.push(S_ENDPGM);
        lower_compute(&w)
    };

    // Float prefix sum (v_add_f32, VOP2 0x03): SignedOpKind must be 0 (Signed).
    let f = prefix_program(0x03);
    assert_eq!(
        f.wave_prefix_f32_args,
        vec![vec![Some(121), None, Some(0), Some(0)]]
    );
    #[cfg(windows)]
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut f.dxil.clone())
            .expect("float wavePrefixOp DXIL validation failed");
    }

    // Unsigned integer prefix sum (v_add_nc_u32, VOP2 0x25): SignedOpKind must be 1 (Unsigned).
    let u = prefix_program(0x25);
    assert_eq!(
        u.wave_prefix_i32_args,
        vec![vec![Some(121), None, Some(0), Some(1)]]
    );
}
