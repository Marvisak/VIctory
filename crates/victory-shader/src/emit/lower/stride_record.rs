//! The tests of the buffer stride records: what a built descriptor's own
//! words name, and how a used-resource record's stride is refused apart
//! from an unknown one (#1482, rule 3).

use super::{Lowered, Parts, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// One compute program through the whole lowering (as `lower::tests`'s
/// helper, which lives in its parent's private tests).
fn lower_with(
    words: &[u32],
    layout: Vec<RootParam>,
    used: Vec<UsedResource>,
) -> Result<Lowered, crate::ShaderError> {
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let sgprs: u32 = layout.iter().map(|p| p.sgprs).sum();
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 256,
        layout,
        used,
        io: StageIo::Cs(ComputeIo {
            // The layout's user SGPRs (at least two), 512 bytes of LDS.
            rsrc2: 1 << 15 | sgprs.max(2) << 1,
            threads: [1, 1, 1],
        }),
        code,
    };
    let p = ir::build(&native.code, false, 256)?;
    let st = structure::structure(&p)?;
    let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
    let io = io::raise(&p, &native)?;
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
}

/// Root parameter `slot`, the user SGPRs from `user_data` on (as
/// `lower::tests`'s helper).
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
/// A stride field of zero, named by the descriptor's own words, is a raw
/// buffer: an element-indexed load reaches the bytes by its offset
/// through a raw view, the index counting nothing.
#[test]
fn a_zero_stride_field_loads_raw_by_element() {
    let words = [
        0xBE82_03C1, // s_mov_b32 s2, -1 (the record count)
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
        0xE030_2000,
        0x8000_0100, // buffer_load_dword v1, v0, s[0:3], 0 idxen
        0x7E0A_0301, // v_mov_b32 v5, v1
        0xD834_0000,
        0x0000_0500, // ds_write_b32 v0, v5
        S_ENDPGM,
    ];
    let used = vec![UsedResource {
        kind: ResourceType::Srv,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }];
    // The record names no stride, so the load lowers only if the
    // descriptor's own field is what the index counts by: left as an element
    // index, it would be refused as a structured buffer of unknown stride.
    let zero = lower_with(&words, vec![root(RootType::Descriptor, 0, 0)], used.clone()).unwrap();
    // The same load with the field at 4 counts four-byte elements: the bytes
    // read come from the field, not from a scale the lowering chose.
    let mut field = vec![0x8801_FF01, 4 << 16]; // s_or_b32 s1, s1, stride 4
    field.extend_from_slice(&words);
    let four = lower_with(&field, vec![root(RootType::Descriptor, 0, 0)], used).unwrap();
    assert_ne!(
        zero.dxil, four.dxil,
        "the index counts by the descriptor's stride field"
    );
}

/// The store of the same raw shape, written by element (the UAV).
#[test]
fn a_zero_stride_field_stores_raw_by_element() {
    let words = [
        0xBE82_03C1, // s_mov_b32 s2, -1 (the record count)
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
        0xE070_2000,
        0x8000_0100, // buffer_store_dword v1, v0, s[0:3], 0 idxen
        S_ENDPGM,
    ];
    // As the load: the record names no stride (none is given at all), so the
    // descriptor's own zero field is what leaves the index counting nothing.
    let zero = lower_with(&words, vec![root(RootType::Descriptor, 0, 0)], vec![]).unwrap();
    // The same store with the field at 4 writes four-byte elements: its bytes
    // come from the field, not from a scale the lowering chose.
    let mut field = vec![0x8801_FF01, 4 << 16]; // s_or_b32 s1, s1, stride 4
    field.extend_from_slice(&words);
    let four = lower_with(&field, vec![root(RootType::Descriptor, 0, 0)], vec![]).unwrap();
    assert_ne!(
        zero.dxil, four.dxil,
        "the index counts by the descriptor's stride field"
    );
}

/// The atomic of the same raw shape, its return read back.
#[test]
fn a_zero_stride_field_atomics_raw_by_element() {
    let words = [
        0xBE82_03C1, // s_mov_b32 s2, -1 (the record count)
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
        0xE0C8_2000,
        0x8000_0100, // buffer_atomic_add v1, v0, s[0:3], 0 idxen
        0x7E0A_0301, // v_mov_b32 v5, v1 (the returned old value)
        0xD834_0000,
        0x0000_0500, // ds_write_b32 v0, v5
        S_ENDPGM,
    ];
    // As the load: the record names no stride (none is given), so the
    // descriptor's own zero field is what addresses the atomic by offset.
    let zero = lower_with(&words, vec![root(RootType::Descriptor, 0, 0)], vec![]).unwrap();
    // The same atomic with the field at 4 addresses four-byte elements: its
    // bytes come from the field, not from a scale the lowering chose.
    let mut field = vec![0x8801_FF01, 4 << 16]; // s_or_b32 s1, s1, stride 4
    field.extend_from_slice(&words);
    let four = lower_with(&field, vec![root(RootType::Descriptor, 0, 0)], vec![]).unwrap();
    assert_ne!(
        zero.dxil, four.dxil,
        "the index counts by the descriptor's stride field"
    );
}

/// A built buffer descriptor's stride field, dword 1 bits 16-29, names
/// the element its index counts — two bytes here — and the accessed
/// bytes are reached through the raw root view either way.
#[test]
fn a_built_stride_field_counts_the_element_the_index_names() {
    let words = |stride: u32| {
        [
            0x8801_FF01,
            stride << 16, // s_or_b32 s1, s1, the stride field
            0xBE82_03C1,  // s_mov_b32 s2, -1 (no record limit)
            0xBE83_03FF,
            0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
            0xE030_2000,
            0x8000_0100, // buffer_load_dword v1, v0, s[0:3], 0 idxen
            0x7E0A_0301, // v_mov_b32 v5, v1
            0xD834_0000,
            0x0000_0500, // ds_write_b32 v0, v5
            S_ENDPGM,
        ]
    };
    let used = vec![UsedResource {
        kind: ResourceType::Srv,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    }];
    // A zero field: a raw buffer, the index counts nothing.
    let zero = lower_with(
        &words(0),
        vec![root(RootType::Descriptor, 0, 0)],
        used.clone(),
    )
    .unwrap();
    // Two bytes and sixteen: the index counts elements of that size, and each
    // value the field names reaches the code the index is scaled in — a
    // lowering that ignored the field would emit one code for all three.
    let two = lower_with(
        &words(2),
        vec![root(RootType::Descriptor, 0, 0)],
        used.clone(),
    )
    .unwrap();
    let sixteen = lower_with(&words(16), vec![root(RootType::Descriptor, 0, 0)], used).unwrap();
    assert_ne!(zero.dxil, two.dxil, "a field of 2 counts two-byte elements");
    assert_ne!(
        zero.dxil, sixteen.dxil,
        "a field of 16 counts 16-byte elements"
    );
    assert_ne!(
        two.dxil, sixteen.dxil,
        "the code scales by the value the field names"
    );
}

/// Swizzle bits above the stride field or a record limit keep a built
/// root view from being indexed — as on the console, a swizzled or
/// limited view is not addressed flat.
#[test]
fn a_built_root_view_with_a_limit_or_swizzle_is_refused_by_index() {
    let words = |w2| {
        [
            0x8801_FF01,
            0x0010_0000, // s_or_b32 s1, s1, stride 16
            0xBE82_03FF,
            w2, // s_mov_b32 s2, the record count
            0xBE83_03FF,
            0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
            0xE030_2000,
            0x8000_0100, // buffer_load_dword v1, v0, s[0:3], 0 idxen
            0x7E0A_0301, // v_mov_b32 v5, v1
            0xD834_0000,
            0x0000_0500, // ds_write_b32 v0, v5
            S_ENDPGM,
        ]
    };
    let e = lower_with(
        &words(0x1000),
        vec![root(RootType::Descriptor, 0, 0)],
        vec![],
    )
    .err()
    .unwrap();
    assert!(e.to_string().contains("swizzling or a record limit"), "{e}");
}

/// Swizzle bits alone (the record count left unlimited) refuse the
/// index the same way — a swizzled view is not addressed flat either.
#[test]
fn a_built_root_view_with_swizzle_bits_alone_is_refused_by_index() {
    let words = [
        0x8801_FF01,
        0x4010_0000, // s_or_b32 s1, s1, stride 16 and swizzle bits
        0xBE82_03C1, // s_mov_b32 s2, -1 (no record limit)
        0xBE83_03FF,
        0x1101_4FAC, // s_mov_b32 s3, (the console's buffer word 3)
        0xE030_2000,
        0x8000_0100, // buffer_load_dword v1, v0, s[0:3], 0 idxen
        0x7E0A_0301, // v_mov_b32 v5, v1
        0xD834_0000,
        0x0000_0500, // ds_write_b32 v0, v5
        S_ENDPGM,
    ];
    let e = lower_with(&words, vec![root(RootType::Descriptor, 0, 0)], vec![])
        .err()
        .unwrap();
    assert!(e.to_string().contains("swizzling or a record limit"), "{e}");
}

/// A stride only the host's view of it names — the record names none, the
/// descriptor's words are runtime data: refused by that, so runs can
/// tell it from an unknown stride. The record's own dword stride
/// still names its elements.
#[test]
fn a_stride_only_the_host_view_names_is_refused_by_that() {
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], null
        0xE030_2000,
        0x8002_0100, // buffer_load_dword v1, v0, s[8:11], 0 idxen
        0x7E0A_0301, // v_mov_b32 v5, v1
        0xD834_0000,
        0x0000_0500, // ds_write_b32 v0, v5
        S_ENDPGM,
    ];
    let used = || {
        vec![UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }]
    };
    let layout = || vec![root(RootType::Table, 0, 0), root(RootType::Constants, 1, 1)];
    let refused = |stride: u32| {
        let mut record = used();
        record[0].stride = stride;
        lower_with(&words, layout(), record).err().unwrap()
    };
    // No record at all, or one that names none (unread FSPX metadata):
    // the element size is the host's view's, known only at runtime.
    let e = lower_with(&words, layout(), used()).err().unwrap();
    assert!(e.to_string().contains("record names no stride"), "{e}");
    // A stride that names no element the host can view.
    for stride in [2, 3, 5, 6] {
        let e = refused(stride);
        assert!(e.to_string().contains("unknown stride"), "{stride}: {e}");
    }
    // The record's own dword stride still names its elements.
    let mut used4 = used();
    used4[0].stride = 4;
    lower_with(&words, layout(), used4).unwrap();
}
