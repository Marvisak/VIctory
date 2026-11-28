use std::collections::HashMap;

use super::{Lower, Parts, Val};
use crate::emit::bitcode::Bin;
use crate::emit::{SLOT_VIEW_V3, SLOT_VIEWS};
use crate::gfx10::{Cache, Fields, NULL, Operand};
use crate::native::{ComputeIo, HwStage, NativeProgram, RootParam, RootType, StageIo};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

fn root(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: if root == RootType::Descriptor { 2 } else { 1 },
        base_register: 0,
        user_data,
        slot,
    }
}

fn view_load() -> (ir::Program, NativeProgram, resource::Resources, ir::Inst) {
    let code: Vec<u8> = [0xFA00_0040u32, 0xF408_0106, S_ENDPGM]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    let mut p = ir::build(&code, false, 256).unwrap();
    let inst = &mut p.blocks[0].insts[0];
    inst.fields = Fields::Smem {
        data: Some(Operand::Scalar { code: 8, dwords: 4 }),
        base: Operand::Scalar { code: 12, dwords: 2 },
        offset: 64,
        soffset: Operand::Scalar { code: NULL, dwords: 1 },
        cache: Cache::default(),
    };
    inst.reads = vec![(ir::Reg::S(12), ir::Value(99))];
    inst.writes = (0..8u8)
        .map(|k| (ir::Reg::S(8 + k), ir::Value(1000 + u32::from(k))))
        .collect();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout: vec![root(RootType::Table, 4, 0)],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 15 | 2 << 1,
            threads: [64, 1, 1],
        }),
        code,
    };
    let res = resource::Resources {
        scalar: vec![(
            (0, 0),
            resource::Address {
                param: 0,
                offset: 64,
                dynamic: Some(ir::Value(77)),
            },
        )],
        ..Default::default()
    };
    let inst = p.blocks[0].insts[0].clone();
    (p, native, res, inst)
}

fn with_view_load<T>(f: impl FnOnce(&mut Lower<'_>, &ir::Inst) -> T) -> T {
    with_view_load_at(64, f)
}

fn with_view_load_at<T>(offset: u32, f: impl FnOnce(&mut Lower<'_>, &ir::Inst) -> T) -> T {
    let (p, native, mut res, mut inst) = view_load();
    res.scalar[0].1.offset = offset;
    if let Fields::Smem { offset: o, .. } = &mut inst.fields {
        *o = offset as i32;
    }
    let st = structure::structure(&p).unwrap();
    let io = io::Io::default();
    let parts = Parts {
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
    };
    let bad = HashMap::new();
    let mut l = Lower::new(&parts, &bad);
    l.vals.insert(ir::Value(99), Val::I32(l.s.m.i32(0x1000)));
    f(&mut l, &inst)
}

#[test]
fn descriptor_load_at_offset_sixty_four_is_not_offset_twice() {
    with_view_load(|l, _| {
        let (lo, _, _, _) = l.view_address_indexed((0, 0), 0).unwrap().unwrap();
        assert!(
            l.s.m.bin_of(lo).is_none(),
            "the view's base address must come straight from the table"
        );
    });
}

#[test]
fn eight_word_descriptor_load_falls_through_to_view_addresses_handle() {
    with_view_load(|l, inst| {
        l.view_addresses(inst, (0, 0), 8, 8).unwrap();
        assert!(l.s.view_addresses.is_some(), "t1 in MEMORY_SPACE is used");
        assert!(
            !l.s.root_srvs.iter().any(|&(slot, _)| slot == SLOT_VIEWS),
            "t252 is only for four-word buffer descriptors"
        );
    });
}

#[test]
fn four_word_descriptor_load_uses_the_hardware_view_layout() {
    with_view_load(|l, inst| {
        l.view_addresses(inst, (0, 0), 8, 4).unwrap();
        let word = |n| match l.vals.get(&ir::Value(1000u32 + n)) {
            Some(Val::I32(v)) => *v,
            _ => panic!("dword {n} was not written"),
        };
        assert_eq!(
            l.s.m.bin_of(word(1)).map(|(op, _, _)| op),
            Some(Bin::Or),
            "V#[1] keeps the address high bits with the packed stride"
        );
        assert_eq!(
            l.s.m.bin_of(word(2)).map(|(op, _, _)| op),
            Some(Bin::UDiv),
            "byte length becomes a record count"
        );
        assert_eq!(
            l.s.m.int_value(word(3)),
            Some(i64::from(SLOT_VIEW_V3)),
            "V#[3] has a valid RDNA2 encoding"
        );
    });
}

#[test]
fn rdna2_slot_view_stride_is_extracted_from_dword_one() {
    with_view_load(|l, inst| {
        l.view_addresses(inst, (0, 0), 8, 4).unwrap();
        let word = |n| match l.vals.get(&ir::Value(1000u32 + n)) {
            Some(Val::I32(v)) => *v,
            _ => panic!("dword {n} was not written"),
        };
        let (_, _, divisor) = l.s.m.bin_of(word(2)).expect("a record count");
        let (_, stride, _) = l.s.m.select_of(divisor).expect("a stride or one");
        let (op, packed, shift) = l.s.m.bin_of(stride).expect("a stride extraction");
        assert_eq!(op, Bin::LShr, "the divisor is read out of V#[1]");
        assert_eq!(l.s.m.int_value(shift), Some(16));
        let (op, _, mask) = l.s.m.bin_of(packed).expect("the stride is masked");
        assert_eq!(op, Bin::And);
        assert_eq!(l.s.m.int_value(mask), Some(i64::from(0x3FFF << 16)));
    });
}

#[test]
fn four_word_descriptor_load_at_offset_sixteen_bypasses_slot_views() {
    with_view_load_at(16, |l, inst| {
        l.view_addresses(inst, (0, 0), 8, 4).unwrap();
        assert!(
            !l.s.root_srvs.iter().any(|&(slot, _)| slot == SLOT_VIEWS),
            "the upper half of an eight-dword image descriptor is not a buffer V#"
        );
        assert!(
            l.s.view_addresses.is_some(),
            "it falls through to the view-address table"
        );
        assert!(
            matches!(l.vals.get(&ir::Value(1000)), Some(Val::Missing(_))),
            "dword 4 is not invented"
        );
    });
}
