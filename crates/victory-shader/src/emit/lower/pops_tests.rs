use super::{Lowered, Parts, RootBinding, classify_root_descriptor, lower};
use crate::native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

fn root(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: match root {
            RootType::Descriptor => 2,
            _ => 1,
        },
        base_register: 0,
        user_data,
        slot,
    }
}

#[test]
fn pops_pixel_shader_lowers_root_uav_accesses_as_rovs() {
    let lower_ps = |words: &[u32], db_shader_control: u32, view_marks: bool| -> Lowered {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let native = NativeProgram {
            stage: HwStage::Ps,
            wave64: false,
            vgprs: 8,
            layout: vec![root(RootType::Descriptor, 8, 0)],
            used: vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 8,
                slot: 0,
            }],
            io: StageIo::Ps {
                inputs: vec![],
                input_addr: 0,
                db_shader_control,
                patches: vec![],
            },
            code,
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
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
            view_marks,
        })
        .expect("POPS root UAV lowering failed")
    };

    // 1) buffer_store_dword through a descriptor built around the root UAV.
    let buffer_store = [
        0xBE80_0308, // s_mov_b32 s0, s8
        0xBE81_0309, // s_mov_b32 s1, s9
        0xBE82_03C1, // s_mov_b32 s2, -1
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, 0x11014fac
        0x7E00_0280, // v_mov_b32 v0, 0
        0xE070_0000,
        0x8000_0000, // buffer_store_dword v0, off, s[0:3], 0
        0xF800_1890,
        0x0000_0000, // exp null off, off, off, off done vm
        S_ENDPGM,
    ];
    let l = lower_ps(&buffer_store, 1 << 16, true);
    assert!(l.rovs, "POPS buffer_store_dword on root UAV must set ROVS");
    assert!(l.view_marks, "POPS root UAV store must still emit write marks");
    assert!(
        l.user_data
            .iter()
            .any(|u| u.dword == super::super::ROOT_UAV_VIEW)
    );
    assert!(
        l.user_data
            .iter()
            .any(|u| u.dword == super::super::ROOT_VIEW_OFFSET)
    );
    assert!(!lower_ps(&buffer_store, 0, false).rovs);

    // 2) global_store_dword directly through the root UAV SGPR pair.
    let global_store = [
        0x7E00_0280, // v_mov_b32 v0, 0
        0x7E02_0281, // v_mov_b32 v1, 1
        0xDC70_8000,
        0x0008_0100, // global_store_dword v0, v1, s[8:9]
        0xF800_1890,
        0x0000_0000, // exp null off, off, off, off done vm
        S_ENDPGM,
    ];
    let l = lower_ps(&global_store, 1 << 16, true);
    assert!(l.rovs, "POPS global_store_dword on root UAV must set ROVS");
    assert!(l.view_marks);

    // 3) global_load_dword on a root UAV passes props.0 = 11 (SRV-like) to
    // handle(), which routes to root_view(..., true) and must still mark ROV.
    let global_load = [
        0x7E00_0280, // v_mov_b32 v0, 0
        0xDC30_8000,
        0x0108_0000, // global_load_dword v1, v0, s[8:9]
        0xF800_1890,
        0x0000_0000, // exp null off, off, off, off done vm
        S_ENDPGM,
    ];
    let l = lower_ps(&global_load, 1 << 16, false);
    assert!(l.rovs, "POPS global_load_dword on root UAV must set ROVS");
    assert!(!lower_ps(&global_load, 0, false).rovs);

    // 4) With `root_memory` set, `Shader::root_uav_handle` has no ROV
    // annotation, so POPS root UAV props (0x2000 set) must stay refused
    // instead of silently lowering as an unordered root UAV handle.
    assert_eq!(
        classify_root_descriptor(
            false,
            RootType::Descriptor,
            Some(ResourceType::Uav),
            0x100B | 0x2000
        ),
        Some(RootBinding::UavView)
    );
    assert_eq!(
        classify_root_descriptor(
            false,
            RootType::Descriptor,
            Some(ResourceType::Uav),
            11 | 0x2000
        ),
        Some(RootBinding::UavView)
    );
    assert_eq!(
        classify_root_descriptor(
            true,
            RootType::Descriptor,
            Some(ResourceType::Uav),
            0x100B
        ),
        Some(RootBinding::UavHandle)
    );
    assert_eq!(
        classify_root_descriptor(
            true,
            RootType::Descriptor,
            Some(ResourceType::Uav),
            0x100B | 0x2000
        ),
        None
    );
    assert_eq!(
        classify_root_descriptor(true, RootType::Descriptor, None, 0x100B | 0x2000),
        None
    );
    assert_eq!(
        classify_root_descriptor(
            true,
            RootType::Descriptor,
            Some(ResourceType::Uav),
            11 | 0x2000
        ),
        None
    );
}
