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

#[test]
fn an_immediate_sampler_selected_by_scc_samples_with_both() {
    // The view table at s0 (2D texture at entry 0, a raw UAV at dword
    // 16) and a root constant at s1: the sampler clamps if s1 is 0,
    // else wraps, chosen dword by dword with `s_cselect_b32`.
    let words = [
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0xF40C_0302,
        0xFA00_0000, // s_load_dwordx8 s[12:19], s[4:5], 0x0
        0xBE98_03FF,
        0x0000_0092, // s_mov_b32 s24, 0x92 (clamp x, y, z)
        0xBE99_0380, // s_mov_b32 s25, 0
        0xBE9A_0380, // s_mov_b32 s26, 0
        0xBE9B_0380, // s_mov_b32 s27, 0
        0xBE94_0380, // s_mov_b32 s20, 0 (wrap)
        0xBE95_0380, // s_mov_b32 s21, 0
        0xBE96_0380, // s_mov_b32 s22, 0
        0xBE97_0380, // s_mov_b32 s23, 0
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0x8514_1418, // s_cselect_b32 s20, s24, s20
        0x8515_1519, // s_cselect_b32 s21, s25, s21
        0x8516_161A, // s_cselect_b32 s22, s26, s22
        0x8517_171B, // s_cselect_b32 s23, s27, s23
        0x7E04_02F0, // v_mov_b32 v2, 0.5
        0x7E06_02F0, // v_mov_b32 v3, 0.5
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xF09C_0F08,
        0x00A3_0C02, // image_sample_lz v[12:15], v[2:3], s[12:19], s[20:23] dmask:0xf dim:SQ_RSRC_IMG_2D
        0xE070_1000,
        0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
        S_ENDPGM,
    ];
    let (_, mut used) = texture_and_uav_tables();
    used.retain(|u| u.user_data == 0);
    let layout = vec![root(RootType::Table, 0, 0), root(RootType::Constants, 1, 1)];
    let l = lower_with(&words, layout, used).unwrap();
    // Static samplers cannot be indexed: the sample runs with both and
    // SCC picks the result.
    assert_eq!(l.samplers.len(), 2);
    assert_eq!(l.nonuniform_handles, 0);
    validate(&l);
}

#[test]
fn nested_uniform_phis_and_selects_choose_descriptors() {
    // A raw read (offen) takes the descriptor's handle; a structured one
    // (idxen) also its stride from the view-stride table.
    for mubuf in [0xE030_1000, 0xE030_2000] {
        let words = [
            0xBE86_0302, // s_mov_b32 s6, s2
            0xBE87_0380, // s_mov_b32 s7, 0
            0xBE8C_0303, // s_mov_b32 s12, s3
            0xBE8D_0380, // s_mov_b32 s13, 0
            0xF408_0406, // s_load_dwordx4 s[16:19], s[12:13], 0x0
            0xFA00_0000,
            0xBF06_8000, // s_cmp_eq_u32 s0, 0
            0xBF85_000F, // s_cbranch_scc1 -> outer taken arm
            0xBF06_8100, // s_cmp_eq_u32 s0, 1
            0xBF85_0003, // s_cbranch_scc1 -> inner select arm
            0xF408_0203, // s_load_dwordx4 s[8:11], s[6:7], 0x20
            0xFA00_0020,
            0xBF82_0009, // s_branch -> inner join
            0xF408_0203, // s_load_dwordx4 s[8:11], s[6:7], 0x40
            0xFA00_0040,
            0xF408_0503, // s_load_dwordx4 s[20:23], s[6:7], 0x60
            0xFA00_0060,
            0xBF06_8001, // s_cmp_eq_u32 s1, 0
            0x8508_0814, // s_cselect_b32 s8, s20, s8
            0x8509_0915, // s_cselect_b32 s9, s21, s9
            0x850A_0A16, // s_cselect_b32 s10, s22, s10
            0x850B_0B17, // s_cselect_b32 s11, s23, s11
            0xBF82_0002, // inner join: s_branch -> outer join
            0xF408_0203, // outer taken: s_load_dwordx4 s[8:11], s[6:7], 0x0
            0xFA00_0000,
            0xBF8C_C07F, // outer join: s_waitcnt lgkmcnt(0)
            mubuf,       // buffer_load_dword v2, v0, s[8:11], 0 (offen / idxen)
            0x8002_0200,
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0x7E02_0280, // v_mov_b32 v1, 0
            0xE070_1000, // buffer_store_dword v2, v1, s[16:19], 0 offen
            0x8004_0201,
            S_ENDPGM,
        ];
        let lowered = lower_with(&words, heap_and_table_layout(), table_uav()).unwrap();
        validate(&lowered);
        assert_eq!(lowered.nonuniform_handles, 0);
        assert!(lowered.heap);
    }
}

#[test]
fn a_descriptor_phi_at_a_loop_header_lowers_its_back_edge() {
    // A raw read (offen) takes the descriptor's handle; a structured one
    // (idxen) also its stride from the view-stride table.
    for mubuf in [0xE030_1000, 0xE030_2000] {
        let words = [
            0xBE86_0302, // s_mov_b32 s6, s2
            0xBE87_0380, // s_mov_b32 s7, 0
            0xBE8C_0303, // s_mov_b32 s12, s3
            0xBE8D_0380, // s_mov_b32 s13, 0
            0xF408_0406, // s_load_dwordx4 s[16:19], s[12:13], 0x0
            0xFA00_0000,
            0xF408_0203, // s_load_dwordx4 s[8:11], s[6:7], 0x0
            0xFA00_0000,
            0xBE84_0300, // s_mov_b32 s4, s0
            0xBF8C_C07F, // loop: s_waitcnt lgkmcnt(0)
            mubuf,       // buffer_load_dword v2, v0, s[8:11], 0 (offen / idxen)
            0x8002_0200,
            0xBF8C_3F70, // s_waitcnt vmcnt(0)
            0xF408_0203, // s_load_dwordx4 s[8:11], s[6:7], 0x20
            0xFA00_0020,
            0x8084_8104, // s_sub_u32 s4, s4, 1
            0xBF07_8004, // s_cmp_lg_u32 s4, 0
            0xBF85_FFF7, // s_cbranch_scc1 -> loop
            0x7E02_0280, // v_mov_b32 v1, 0
            0xE070_1000, // buffer_store_dword v2, v1, s[16:19], 0 offen
            0x8004_0201,
            S_ENDPGM,
        ];
        let lowered = lower_with(&words, heap_and_table_layout(), table_uav()).unwrap();
        validate(&lowered);
        assert_eq!(lowered.nonuniform_handles, 0);
    }
}
