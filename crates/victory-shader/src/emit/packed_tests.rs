//! Synthetic opaque pixel export used to check the packed integer variant.

use super::{Options, container, validator};
use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

#[test]
fn packed_shared_exp_pixel_has_uint_target0_and_valid_dxil() {
    // RGB includes a nearest-even tie (1 + 1/512). The native output is
    // one complete colour export, as in the two observed title shaders.
    let words = [
        0x7E00_02FF,
        0x3F80_4000, // v0 = red, exactly 1 + 1/512 (nearest-even tie)
        0x7E02_02FF,
        0.5f32.to_bits(), // v1 = green
        0x7E04_02FF,
        0.25f32.to_bits(), // v2 = blue
        0xF800_180F,
        0x0202_0100, // exp mrt0 v0,v1,v2,v2 done vm
        0xBF81_0000, // s_endpgm
    ];
    let native = NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 3,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Ps {
            inputs: Vec::new(),
            input_addr: 0,
            db_shader_control: 0,
            patches: Vec::new(),
        },
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    };
    let osg1 = container::signature(
        &[container::SigElement {
            name: "SV_Target".into(),
            index: 0,
            kind: 16,
            comp: 9,
            interp: 0,
            row: Some(0),
            col: 0,
            cols: 4,
            usage: 15,
            half: false,
        }],
        false,
    );
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let packed = super::emit_with(
        &c,
        &native,
        &p,
        &st,
        &io,
        &res,
        Options {
            packed_shared_exp_target0: true,
            ..Options::default()
        },
    )
    .unwrap();
    let output = Container::parse(&packed.dxil).unwrap();
    let sig = output.part(FourCC::OSG1).unwrap().data;
    assert_eq!(u32::from_le_bytes(sig[0..4].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(sig[24..28].try_into().unwrap()), 1); // uint
    assert_eq!(sig[32], 1); // only the red register component is written
    if let Ok(mut check) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        check.validate(&mut packed.dxil.clone()).unwrap();
    }
}
