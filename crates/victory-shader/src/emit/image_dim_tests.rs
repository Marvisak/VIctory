//! End-to-end tests of image accesses by dimension: synthetic GFX10
//! programs that load, store and update texels of cube faces and load
//! samples of multisampled textures, lowered to DXIL, validated by
//! `dxil.dll` when present, and run on the default hardware adapter and
//! WARP; each thread stores what it read to a raw UAV.

use super::host::{Dispatch, Host, Root, View};
use super::sampler::StaticSampler;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
/// `s_waitcnt lgkmcnt(0)`.
const WAIT_SCALAR: u32 = 0xBF8C_C07F;
/// `s_waitcnt vmcnt(0)`.
const WAIT_VECTOR: u32 = 0xBF8C_3F70;

/// MIMG opcodes.
const IMAGE_LOAD: u32 = 0x00;
const IMAGE_LOAD_MIP: u32 = 0x01;
const IMAGE_STORE: u32 = 0x08;
const IMAGE_GET_RESINFO: u32 = 0x0E;
const IMAGE_ATOMIC_ADD: u32 = 0x11;
/// MIMG dimensions.
const CUBE: u32 = 3;
const MSAA: u32 = 6;
const MSAA_ARRAY: u32 = 7;

/// SOP1 `s_mov_b32 sdst, src0`.
const fn s_mov_b32(sdst: u8, src0: u8) -> u32 {
    0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | src0 as u32
}

/// SMEM `s_load_dwordx{4,8} s[dst..], s[base:base+1], byte_offset`.
const fn s_load(dwords: u8, dst: u8, base: u8, byte_offset: u32) -> [u32; 2] {
    let op = if dwords == 8 { 3 } else { 2 };
    [
        0x3D << 26 | op << 18 | (dst as u32) << 6 | (base as u32 >> 1),
        125 << 25 | byte_offset,
    ]
}

/// VOP2 `op vdst, src0, vsrc1` (`src0`: an inline constant or 256 + VGPR).
const fn vop2(op: u32, vdst: u8, src0: u16, vsrc1: u8) -> u32 {
    op << 25 | (vdst as u32) << 17 | (vsrc1 as u32) << 9 | src0 as u32
}

/// VOP1 `op vdst, src0`.
const fn vop1(op: u32, vdst: u8, src0: u16) -> u32 {
    0x7E00_0000 | (vdst as u32) << 17 | op << 9 | src0 as u32
}

const fn v(n: u8) -> u16 {
    256 + n as u16
}

const V_MOV_B32: u32 = 0x01;
const V_CVT_F32_U32: u32 = 0x06;
const V_LSHRREV_B32: u32 = 0x16;
const V_LSHLREV_B32: u32 = 0x1A;
const V_AND_B32: u32 = 0x1B;
const V_ADD_NC_U32: u32 = 0x25;

/// An inline integer constant (0..=64).
const fn k(n: u16) -> u16 {
    128 + n
}

/// MIMG (non-NSA) `op v[vdata..], v[vaddr..], s[8:15]`, with `glc` (an
/// atomic's old value returned).
const fn mimg_glc(op: u32, dim: u32, dmask: u32, vdata: u8, vaddr: u8, glc: bool) -> [u32; 2] {
    [
        0x3C << 26 | (op & 0x7F) << 18 | (glc as u32) << 13 | dmask << 8 | dim << 3 | op >> 7,
        (8 >> 2) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// MIMG `op v[vdata..], v[vaddr..]` on the image whose descriptor is in
/// s[8:15].
const fn mimg(op: u32, dim: u32, dmask: u32, vdata: u8, vaddr: u8) -> [u32; 2] {
    mimg_glc(op, dim, dmask, vdata, vaddr, false)
}

/// MUBUF `buffer_store_dwordx4 v[vdata:vdata+3], v[vaddr], s[16:19], 0
/// offen`: to the raw UAV.
const fn store_x4(vdata: u8, vaddr: u8) -> [u32; 2] {
    [
        0x38 << 26 | 0x1E << 18 | 1 << 12,
        0x80 << 24 | (16 >> 2) << 16 | (vdata as u32) << 8 | vaddr as u32,
    ]
}

/// The image's descriptor (8 dwords at table entry 0) to s[8:15], the raw
/// UAV's (entry 1) to s[16:19], each thread's byte offset in the raw UAV
/// (16 per thread) to v20; then `body`, then `image`, then v[4:7] to the
/// raw UAV.
fn program(body: &[u32], image: &[u32]) -> Vec<u32> {
    let mut w = vec![s_mov_b32(4, 0), s_mov_b32(5, 0x80)];
    w.extend(s_load(8, 8, 4, 0));
    w.extend(s_load(4, 16, 4, 0x20));
    w.push(vop2(V_LSHLREV_B32, 20, k(4), 0));
    w.extend(body);
    w.push(WAIT_SCALAR);
    w.extend(image);
    w.push(WAIT_VECTOR);
    w.extend(store_x4(4, 20));
    w.push(S_ENDPGM);
    w
}

/// Lowers a wave32 compute program of `threads` threads with the table of
/// views at s0: the image (an SRV, or a UAV with `uav_image`) at entry 0,
/// the raw UAV at entry 1.
fn build(words: &[u32], threads: u32, uav_image: bool) -> HostShader {
    let range = |kind, table_offset_dwords, dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords,
        user_data: 0,
        slot: 0,
    };
    let image = if uav_image {
        ResourceType::Uav
    } else {
        ResourceType::Srv
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![range(image, 0, 8), range(ResourceType::Uav, 8, 4)],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        view_marks: false,
    })
    .expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        static_samplers: lowered
            .samplers
            .iter()
            .map(|w| StaticSampler::from_words(*w))
            .collect(),
        samplers: lowered.samplers,
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        memory: lowered.memory,
        memory_writes: lowered.memory_writes,
        view_addresses: lowered.view_addresses,
        heap: lowered.heap,
        heap_writes: lowered.heap_writes,
        view_marks: lowered.view_marks,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// Runs `shader` with `image` at heap index 0 and a raw UAV of 16 bytes
/// per thread at 1, on the default hardware adapter and on WARP (each
/// skipped without a device); per device that ran, the UAVs' contents in
/// heap order.
fn run(shader: &HostShader, image: View, threads: u32) -> Vec<(&'static str, Vec<Vec<u32>>)> {
    let views = [image, View::RawUav(4 * threads)];
    let mut out = Vec::new();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            assert_eq!(u.root, RootType::Table, "{u:?}");
            ud[u.index as usize] = 0;
        }
        let uavs = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, uavs));
    }
    assert!(!out.is_empty(), "no device ran");
    out
}

fn bits(texels: &[[f32; 4]]) -> Vec<u32> {
    texels.iter().flatten().map(|f| f.to_bits()).collect()
}

/// A distinct texel per face, level, column and row of a cube.
fn cube_texel(face: u32, level: u32, x: u32, y: u32) -> [f32; 4] {
    let id = 1000 * level + 16 * face + 4 * y + x;
    [face as f32, x as f32, y as f32, id as f32 + 0.5]
}

/// v1 = x = t & 3, v2 = y = t >> 2 & 3, v3 = face = t >> 4: thread t's
/// texel of a 4x4 cube.
const CUBE_ADDRESS: [u32; 4] = [
    vop2(V_AND_B32, 1, k(3), 0),
    vop2(V_LSHRREV_B32, 2, k(2), 0),
    vop2(V_AND_B32, 2, k(3), 2),
    vop2(V_LSHRREV_B32, 3, k(4), 0),
];

/// Thread t loads its texel (`CUBE_ADDRESS`) of a 4x4 cube SRV: all 96, in
/// the console's face order (+x -x +y -y +z -z as slices 0 to 5).
#[test]
fn cube_loads_read_each_faces_texels() {
    let shader = build(
        &program(&CUBE_ADDRESS, &mimg(IMAGE_LOAD, CUBE, 0xF, 4, 1)),
        96,
        false,
    );
    let texels: Vec<[f32; 4]> = (0..96)
        .map(|t| cube_texel(t / 16, 0, t % 4, t / 4 % 4))
        .collect();
    for (label, uavs) in run(&shader, View::Cube(4, &texels), 96) {
        assert_eq!(uavs[0], bits(&texels), "{label}");
    }
}

/// Thread t loads texel (t & 1, t >> 1 & 1) of face t >> 2 at level 1 of
/// a two-level 4x4 cube SRV, some channels only.
#[test]
fn cube_mip_loads_read_the_level_asked_for() {
    let body = [
        vop2(V_AND_B32, 12, k(1), 0),
        vop2(V_LSHRREV_B32, 13, k(1), 0),
        vop2(V_AND_B32, 13, k(1), 13),
        vop2(V_LSHRREV_B32, 14, k(2), 0),
        vop1(V_MOV_B32, 15, k(1)),
        // Channels not loaded stay as set here.
        vop1(V_MOV_B32, 6, k(7)),
        vop1(V_MOV_B32, 7, k(7)),
    ];
    // Red and alpha: to v4 and v5.
    let shader = build(
        &program(&body, &mimg(IMAGE_LOAD_MIP, CUBE, 0x9, 4, 12)),
        24,
        false,
    );
    let mut texels = Vec::new();
    for face in 0..6 {
        for (level, n) in [(0, 4), (1, 2)] {
            texels.extend((0..n * n).map(|i| cube_texel(face, level, i % n, i / n)));
        }
    }
    let want: Vec<u32> = (0..24)
        .flat_map(|t| {
            let [r, _, _, a] = cube_texel(t / 4, 1, t & 1, t >> 1 & 1);
            [r.to_bits(), a.to_bits(), 7, 7]
        })
        .collect();
    for (label, uavs) in run(&shader, View::MippedCube(4, 2, &texels), 24) {
        assert_eq!(uavs[0], want, "{label}");
    }
}

/// Thread t stores (t, x, y, face) to its texel (`CUBE_ADDRESS`) of a
/// cube's 2D-array UAV (the only unordered view D3D12 has of a cube), then
/// loads it back: face f lands in slice f.
#[test]
fn cube_stores_and_unordered_loads_address_faces_as_slices() {
    let mut body = CUBE_ADDRESS.to_vec();
    for r in 0..4 {
        body.push(vop1(V_CVT_F32_U32, 8 + r, v(r)));
    }
    let mut image = mimg(IMAGE_STORE, CUBE, 0xF, 8, 1).to_vec();
    image.push(WAIT_VECTOR);
    image.extend(mimg(IMAGE_LOAD, CUBE, 0xF, 4, 1));
    let shader = build(&program(&body, &image), 96, true);
    // Slice by slice, row by row: thread t's texel is the t-th.
    let want: Vec<u32> = (0..96u32)
        .flat_map(|t| [t, t & 3, t >> 2 & 3, t >> 4].map(|x| (x as f32).to_bits()))
        .collect();
    for (label, uavs) in run(&shader, View::TextureArrayUav(4, 4, 6), 96) {
        assert_eq!(uavs[0], want, "{label}: the cube");
        assert_eq!(uavs[1], want, "{label}: what the threads loaded");
    }
}

/// Thread t adds t + 1 to its texel (`CUBE_ADDRESS`) of a cube's R32_UINT
/// 2D-array UAV, the old value returned (to v4, the rest of v[4:7] zero).
#[test]
fn cube_atomics_update_each_faces_texels() {
    let mut body = CUBE_ADDRESS.to_vec();
    body.push(vop2(V_ADD_NC_U32, 4, k(1), 0));
    for r in 5..8 {
        body.push(vop1(V_MOV_B32, r, k(0)));
    }
    let shader = build(
        &program(&body, &mimg_glc(IMAGE_ATOMIC_ADD, CUBE, 0x1, 4, 1, true)),
        96,
        true,
    );
    for (label, uavs) in run(&shader, View::UintTextureArrayUav(4, 4, 6), 96) {
        assert_eq!(uavs[0], (1..=96).collect::<Vec<u32>>(), "{label}: the cube");
        assert_eq!(uavs[1], vec![0; 384], "{label}: the old values");
    }
}

/// Sample s of pixel (x, y) of slice z of a 4x4 four-sample texture:
/// (1 + its index) / 256, exact as a depth.
fn sample_value(z: u32, x: u32, y: u32, s: u32) -> f32 {
    (1 + ((z * 4 + y) * 4 + x) * 4 + s) as f32 / 256.0
}

fn msaa_values(slices: u32) -> Vec<f32> {
    (0..slices * 64)
        .map(|i| sample_value(i / 64, i / 4 % 4, i / 16 % 4, i % 4))
        .collect()
}

/// Thread t loads sample t >> 4 of pixel (t & 3, t >> 2 & 3) of a 4x4
/// four-sample texture: all 64.
#[test]
fn multisampled_loads_read_the_sample_asked_for() {
    let shader = build(
        &program(&CUBE_ADDRESS, &mimg(IMAGE_LOAD, MSAA, 0xF, 4, 1)),
        64,
        false,
    );
    let values = msaa_values(1);
    let view = View::MsaaDepth {
        width: 4,
        height: 4,
        samples: 4,
        slices: 1,
        array: false,
        values: &values,
    };
    let want: Vec<[f32; 4]> = (0..64)
        .map(|t| [sample_value(0, t & 3, t >> 2 & 3, t >> 4), 0.0, 0.0, 1.0])
        .collect();
    for (label, uavs) in run(&shader, view, 64) {
        assert_eq!(uavs[0], bits(&want), "{label}");
    }
}

/// Thread t loads sample t >> 5 of pixel (t & 3, t >> 2 & 3) of slice
/// t >> 4 & 1 of a two-slice 4x4 four-sample array, red only.
#[test]
fn multisampled_array_loads_read_the_slice_and_sample_asked_for() {
    let body = [
        vop2(V_AND_B32, 12, k(3), 0),
        vop2(V_LSHRREV_B32, 13, k(2), 0),
        vop2(V_AND_B32, 13, k(3), 13),
        vop2(V_LSHRREV_B32, 14, k(4), 0),
        vop2(V_AND_B32, 14, k(1), 14),
        vop2(V_LSHRREV_B32, 15, k(5), 0),
        // Channels not loaded stay as set here.
        vop1(V_MOV_B32, 5, k(7)),
        vop1(V_MOV_B32, 6, k(7)),
        vop1(V_MOV_B32, 7, k(7)),
    ];
    let shader = build(
        &program(&body, &mimg(IMAGE_LOAD, MSAA_ARRAY, 0x1, 4, 12)),
        128,
        false,
    );
    let values = msaa_values(2);
    let view = View::MsaaDepth {
        width: 4,
        height: 4,
        samples: 4,
        slices: 2,
        array: true,
        values: &values,
    };
    let want: Vec<u32> = (0..128)
        .flat_map(|t| {
            let red = sample_value(t >> 4 & 1, t & 3, t >> 2 & 3, t >> 5);
            [red.to_bits(), 7, 7, 7]
        })
        .collect();
    for (label, uavs) in run(&shader, view, 128) {
        assert_eq!(uavs[0], want, "{label}");
    }
}

/// The width, height and layers of a multisampled 2D texture and array.
#[test]
fn multisampled_sizes() {
    let body = [vop1(V_MOV_B32, 1, k(0)), vop1(V_MOV_B32, 7, k(7))];
    let values = msaa_values(2);
    for (dim, array, layers) in [(MSAA, false, 1), (MSAA_ARRAY, true, 2)] {
        let shader = build(
            &program(&body, &mimg(IMAGE_GET_RESINFO, dim, 0x7, 4, 1)),
            1,
            false,
        );
        let view = View::MsaaDepth {
            width: 4,
            height: 4,
            samples: 4,
            slices: 2,
            array,
            values: &values,
        };
        for (label, uavs) in run(&shader, view, 1) {
            assert_eq!(uavs[0], [4, 4, layers, 7], "{label}, dim {dim}");
        }
    }
}
