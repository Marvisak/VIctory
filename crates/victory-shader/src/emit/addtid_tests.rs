//! Synthetic lane-indexed memory tests assembled for GFX1030.

use super::HostShader;
use super::host::{Dispatch, Host, Root, View};
use super::valu_tests::{ONE_AT_A_TIME, lower_synthetic_cs};
use crate::dxbc::{Container, FourCC, u32_at};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

const THREADS: u32 = 128;
const LDS_OFFSET: u32 = 260;
const LDS_DWORDS: u32 = 128;
const INIT_LDS: &[u32] = &[
    0x4a04_0081, // v_add_nc_u32 v2, 1, v0
    0xd834_0000,
    0x0000_0201, // ds_write_b32 v1, v2
    0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
    0xbf8a_0000, // s_barrier
];

fn build(body: &[u32], wave64: bool, lds: bool, srv: bool) -> HostShader {
    let root = |root, user_data, slot, sgprs| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let used = |kind, root, user_data, slot| UsedResource {
        kind,
        root,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data,
        slot,
    };
    let mut layout = vec![root(RootType::Table, 4, 1, 1)];
    let mut used_resources = vec![used(ResourceType::Uav, RootType::Table, 4, 1)];
    if srv {
        layout.push(root(RootType::Descriptor, 6, 2, 2));
        used_resources.push(used(ResourceType::Srv, RootType::Descriptor, 6, 2));
    } else {
        layout.push(root(RootType::Constants, 0, 0, 2));
    }
    let mut words = vec![
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
        0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
    ];
    words.extend_from_slice(body);
    words.extend([0xe070_1000, 0x8002_0301]); // buffer_store_dword v3, v1, s[8:11], 0 offen
    if srv {
        words.extend([0xe070_1200, 0x8002_0401]); // buffer_store_dword v4, v1, ... offset:512
    }
    words.push(0xbf81_0000); // s_endpgm
    lower_synthetic_cs(&NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 8,
        layout,
        used: used_resources,
        io: StageIo::Cs(ComputeIo {
            rsrc2: ((if srv { 8 } else { 5 }) << 1) | (u32::from(lds) << 15),
            threads: [THREADS, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    })
}

fn run(
    shader: &HostShader,
    wave: u32,
    srv: Option<&[u32]>,
    cases: &[[u32; 2]],
    reference: impl Fn([u32; 2]) -> Vec<u32>,
) {
    assert_eq!(wave_bounds(shader), [0, u32::MAX]);
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(shader).expect("compute pipeline");
        for &case in cases {
            let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                ud[u.index as usize] = match (u.root, u.slot) {
                    (RootType::Constants, 0) => case[u.dword as usize],
                    (RootType::Table, 1) => 0,
                    other => panic!("unexpected user data {other:?}"),
                };
            }
            let mut roots = vec![(0, Root::Constants(ud))];
            if let Some(data) = srv {
                assert_eq!(shader.root_srvs, [2]);
                roots.push((
                    1,
                    Root::View {
                        srv: true,
                        data: data.to_vec(),
                    },
                ));
            }
            let expected = reference(case);
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots,
                        views: &[View::RawUav(expected.len() as u32)],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(out[0], expected, "{label}, wave{wave}, {case:08x?}");
        }
        ran += 1;
        eprintln!(
            "{label}, guest wave{wave}: {} ADDTID dispatch(es) matched",
            cases.len()
        );
    }
    assert!(ran > 0, "neither backend can dispatch the shader");
}

fn wave_bounds(shader: &HostShader) -> [u32; 2] {
    let container = Container::parse(&shader.dxil).unwrap();
    let info = container.part(FourCC::PSV0).unwrap().data;
    // PSV0 starts with the runtime-info size. The lane counts are at 16/20.
    [u32_at(info, 4 + 16).unwrap(), u32_at(info, 4 + 20).unwrap()]
}

fn exec_ops(wave64: bool) -> (u32, u32) {
    if wave64 {
        (0xbe90_047e, 0xbefe_0410) // s_mov_b64 s[16:17], exec / s_mov_b64 exec, s[16:17]
    } else {
        (0xbe90_037e, 0xbefe_0310) // s_mov_b32 s16, exec_lo / s_mov_b32 exec_lo, s16
    }
}

#[test]
fn ds_read_addtid_uses_m0_offset_and_preserves_inactive_lanes() {
    for wave in [32, 64] {
        let (save, restore) = exec_ops(wave == 64);
        for lds in [true, false] {
            let mut body = if lds { INIT_LDS.to_vec() } else { Vec::new() };
            body.extend([
                0xbefc_0300, // s_mov_b32 m0, s0
                0x7e06_02b7, // v_mov_b32 v3, 55
                save,
                0x7daa_0080, // v_cmpx_ne_u32 0, v0
                0xdac4_0104,
                0x0300_0000, // ds_read_addtid_b32 v3 offset:260
                0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
                restore,
            ]);
            let shader = build(&body, wave == 64, lds, false);
            let cases = [[0xabcd_0004, 0], [0x1234_00e8, 0]];
            run(&shader, wave, None, &cases, |[m0, _]| {
                let start = ((m0 & 0xffff) + LDS_OFFSET) / 4;
                (0..THREADS)
                    .map(|tid| {
                        let dword = start + tid % wave;
                        if tid == 0 {
                            55
                        } else if lds && dword < LDS_DWORDS {
                            dword + 1
                        } else {
                            0
                        }
                    })
                    .collect()
            });
        }
    }
}

#[test]
fn ds_write_addtid_updates_only_active_in_bounds_dwords() {
    for wave in [32, 64] {
        let (save, restore) = exec_ops(wave == 64);
        let mut body = INIT_LDS.to_vec();
        body.extend([
            0xbefc_0300, // s_mov_b32 m0, s0
            0x4a06_00ff,
            256, // v_add_nc_u32 v3, 256, v0
            save,
            0x7da6_0001, // v_cmpx_le_u32 s1, v0
            0xdac0_0104,
            0x0000_0300, // ds_write_addtid_b32 v3 offset:260
            0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
            restore,
            0xbf8a_0000, // s_barrier
            0xd8d8_0000,
            0x0300_0001, // ds_read_b32 v3, v1
            0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
        ]);
        let shader = build(&body, wave == 64, true, false);
        // Only the end of the last wave writes, so no two active lanes alias.
        let threshold = THREADS - wave + 3;
        let cases = [[0x4321_0004, threshold], [0x1234_00e8, threshold]];
        run(&shader, wave, None, &cases, |[m0, threshold]| {
            let start = ((m0 & 0xffff) + LDS_OFFSET) / 4;
            let mut expected: Vec<_> = (1..=LDS_DWORDS).collect();
            for tid in threshold..THREADS {
                if let Some(cell) = expected.get_mut((start + tid % wave) as usize) {
                    *cell = 256 + tid;
                }
            }
            expected
        });
    }
}

#[test]
fn ds_addtid_without_a_group_barrier_keeps_guest_wave_size() {
    for wave in [32, 64] {
        let shader = build(
            &[
                0xbefc_0300, // s_mov_b32 m0, s0
                0xdac0_0000,
                0x0000_0100, // ds_write_addtid_b32 v1
                0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
                0xdac4_0004,
                0x0300_0000, // ds_read_addtid_b32 v3 offset:4
                0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
            ],
            wave == 64,
            true,
            false,
        );
        assert_eq!(wave_bounds(&shader), [wave; 2]);
    }
}

#[test]
fn global_load_addtid_reads_dwords_with_signed_offsets() {
    let data: Vec<_> = (0..72).map(|i| 0x7fc0_0000 ^ (i * 131_071)).collect();
    let data = data.as_slice();
    for wave in [32, 64] {
        let (save, restore) = exec_ops(wave == 64);
        let shader = build(
            &[
                0x8006_8c06, // s_add_u32 s6, s6, 12
                0x8207_8007, // s_addc_u32 s7, s7, 0
                0x7e06_02b7, // v_mov_b32 v3, 55
                0x7e08_02b8, // v_mov_b32 v4, 56
                save,
                0x7daa_0080, // v_cmpx_ne_u32 0, v0
                0xdc58_8ff8,
                0x0306_0000, // global_load_dword_addtid v3, s[6:7] offset:-8
                0xdc58_8004,
                0x0406_0000, // global_load_dword_addtid v4, s[6:7] offset:4
                0xbf8c_3f70, // s_waitcnt vmcnt(0)
                restore,
            ],
            wave == 64,
            false,
            true,
        );
        run(&shader, wave, Some(data), &[[0; 2]], |_| {
            [(1, 55), (4, 56)]
                .into_iter()
                .flat_map(|(base, old)| {
                    (0..THREADS).map(move |tid| {
                        if tid == 0 {
                            old
                        } else {
                            data[(base + tid % wave) as usize]
                        }
                    })
                })
                .collect()
        });
    }
}
