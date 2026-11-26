//! Lowering tests of heap descriptors: a child of `lower`'s tests, whose
//! helpers it takes.

use super::*;

/// Root constants at s0-s1, the resource heap at s2 and a view table at
/// s3 (the program's output).
fn heap_and_table_layout() -> Vec<RootParam> {
    vec![
        RootParam {
            sgprs: 2,
            ..root(RootType::Constants, 0, 0)
        },
        root(RootType::ResourceHeap, 2, 1),
        root(RootType::Table, 3, 2),
    ]
}

/// The raw UAV at the start of [`heap_and_table_layout`]'s table.
fn table_uav() -> Vec<UsedResource> {
    vec![UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 3,
        slot: 2,
    }]
}

/// Reads at a heap index taken from data clamp it to the last slot
/// before indexing the view-stride table or the view addresses, root
/// views with no bounds of their own.
#[test]
fn heap_indices_from_data_are_clamped_before_root_view_reads() {
    // The heap descriptor at index s0 (s4 = s0 * 32 bytes), then one
    // read: `access` (whose result is in v2), stored to the table's UAV.
    let program = |load: [u32; 2], access: &[u32]| {
        let mut w = vec![
            0x8F04_8500, // s_lshl_b32 s4, s0, 5
            0xBE86_0302, // s_mov_b32 s6, s2
            0xBE87_0380, // s_mov_b32 s7, 0
            load[0],     // s_load_dwordx{2,4} s[8:..], s[6:7], s4
            load[1],
            0xBE8C_0303, // s_mov_b32 s12, s3
            0xBE8D_0380, // s_mov_b32 s13, 0
            0xF408_0406, // s_load_dwordx4 s[16:19], s[12:13], 0x0
            0xFA00_0000,
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        ];
        w.extend_from_slice(access);
        w.extend_from_slice(&[
            0x7E02_0280, // v_mov_b32 v1, 0
            0xE070_1000, // buffer_store_dword v2, v1, s[16:19], 0 offen
            0x8004_0201,
            S_ENDPGM,
        ]);
        let l = lower_with(&w, heap_and_table_layout(), table_uav()).unwrap();
        validate(&l);
        l
    };
    let x4 = [0xF408_0203, 0x0800_0000];
    let read = |mubuf: u32| {
        [
            mubuf, // buffer_load_dword v2, v0, s[8:11], 0 (offen or idxen)
            0x8002_0200,
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
        ]
    };
    let raw = program(x4, &read(0xE030_1000));
    let structured = program(x4, &read(0xE030_2000));
    // Read by element, the view's stride comes from `VIEW_STRIDES`:
    // one more clamp than the raw read, whose guard clamps already.
    assert_eq!(structured.view_clamps, raw.view_clamps + 1);

    // Dwords 0 and 1 read as data (a buffer's address): two lookups in
    // the view addresses, each clamped.
    let none = program(x4, &[0x7E04_0280]); // v_mov_b32 v2, 0
    let address = program(
        [0xF404_0203, 0x0800_0000], // s_load_dwordx2 s[8:9], s[6:7], s4
        &[
            0x800A_0908, // s_add_u32 s10, s8, s9
            0x7E04_020A, // v_mov_b32 v2, s10
        ],
    );
    assert!(address.view_addresses);
    assert_eq!(address.view_clamps, none.view_clamps + 2);
}
