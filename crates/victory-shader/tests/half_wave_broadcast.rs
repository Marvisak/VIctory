//! A whole-wave reduction can leave its result in a VGPR or an SGPR.
use victory_shader::dxbc::{Container, FourCC};
use victory_shader::native::{ComputeIo, HwStage, NativeProgram, StageIo};
use victory_shader::{emit, io, ir, resource, structure};

/// Copy the lane-31 reduction result into v3 before or after restoring EXEC.
/// Both versions store the same dword (including lane 31's high half) to LDS.
fn translate(copy_inside: bool) -> Result<Vec<u8>, victory_shader::ShaderError> {
    let mut words = vec![
        0x3408_0090u32, // v_lshlrev_b32 v4, 16, v0: lane ID in the high half
        0x3808_08ff,
        0x3c00,      // v_or_b32 v4, 0x3c00, v4: 1.0h in the low half
        0x340a_0082, // v_lshlrev_b32 v5, 2, v0: dword-aligned LDS address
        0xbe80_3dc1, // s_or_saveexec_b32 s0, -1
        0xd501_0001,
        0x0002_0880, // v_cndmask_b32 v1, 0, v4, s0
    ];
    for ctrl in [0xb1, 0x4e, 0x164, 0x168] {
        // v_add_f16 v1, v1.dpp, v1: sum within each row.
        words.extend([0x6402_02fa, 0xff00_0001 | ctrl << 8]);
    }
    words.extend([
        0xd778_0002,
        0x0201_0101, // v_permlanex16_b32 v2, v1, 0, 0
        0x6402_0302, // v_add_f16 v1, v2, v1
        0xd760_0001,
        0x0001_3f01, // v_readlane_b32 s1, v1, 31
    ]);
    let copy = 0x7e06_0201; // v_mov_b32 v3, s1
    let restore = 0xbefe_0300; // s_mov_b32 exec_lo, s0
    words.extend(if copy_inside {
        [copy, restore]
    } else {
        [restore, copy]
    });
    words.extend([0xd834_0000, 0x0000_0305, 0xbf81_0000]); // ds_write_b32 v5, v3; s_endpgm

    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 6,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15,
            threads: [32, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    };
    let p = ir::build(&native.code, false, native.vgprs)?;
    let st = structure::structure(&p)?;
    let resources = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
    let container = Container {
        digest: [0; 16],
        version: (1, 0),
        parts: vec![],
    };
    Ok(emit::emit(&container, &native, &p, &st, &io, &resources)?.dxil)
}

#[test]
fn a_half_reduction_copied_before_exec_restore_reaches_the_lds_store() {
    let scalar = translate(false).expect("scalar live-out");
    let vector = translate(true).expect("broadcast live-out");
    assert!(
        Container::parse(&vector)
            .unwrap()
            .part(FourCC::DXIL)
            .is_some()
    );
    assert_eq!(vector, scalar, "both copies must emit the same shader");
}
