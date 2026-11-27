//! A pixel program's level of detail at a triangle edge and after a
//! discard, without the console's compiler (`diff_tests`' "level of
//! detail" case needs the console SDK's `dxc.exe`). `image_get_lod` takes
//! the derivatives of the coordinates across each quad, so helper lanes
//! (outside the triangle, or killed by a discard) must have computed
//! theirs, or the level of the live pixels beside them is wrong.

use windows::Win32::Graphics::Direct3D12::*;

use super::container::{SigElement, signature};
use super::host::{Draw, Host, Root};
use super::{HostShader, emit};
use crate::native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, Usage, UsedResource,
    VsInput,
};
use crate::{Container, FourCC, io, ir, resource, structure};

const EXEC_LO: u32 = 0x7E;
const S_ENDPGM: u32 = 0xBF81_0000;
const fn sop1(op: u32, d: u32, s: u32) -> u32 {
    0xBE80_0000 | d << 16 | op << 8 | s
}
const fn sop2(op: u32, d: u32, a: u32, b: u32) -> u32 {
    0x8000_0000 | op << 23 | d << 16 | b << 8 | a
}
const fn vop1(op: u32, d: u32, s: u32) -> u32 {
    0x7E00_0000 | d << 17 | op << 9 | s
}
const fn vop2(op: u32, d: u32, s0: u32, v1: u32) -> u32 {
    op << 25 | d << 17 | v1 << 9 | s0
}
const fn v(n: u32) -> u32 {
    256 + n
}

/// The texture's size; one level.
const SIZE: u32 = 16;
/// The coordinates are `u = a * x`, `v = b * y` of the pixel position, so
/// the derivatives are `a` and `b` everywhere and the level is
/// `log2(SIZE * max(a, b))` (isotropic) at every pixel.
const A: f32 = 0.3;
const B: f32 = 0.11;

fn level() -> f32 {
    (SIZE as f32 * A.max(B)).log2()
}

/// Whether the discarding program kills pixel (x, y).
fn killed(x: u32, y: u32) -> bool {
    (7 * x + 3 * y) & 5 == 5
}

/// Loads the texture's descriptor (table at user data 4) into s[8:15] and
/// the sampler's (table at user data 6) into s[16:19], computes u and v in
/// v2 and v3, optionally discards (the console's form, as in
/// `quad_read_tests`, the live mask in s20), and exports `image_get_lod`'s clamped and unclamped
/// levels with 1.0 in alpha (drawn).
fn pixel_program(wave64: bool, discard: bool) -> Vec<u32> {
    let (mov, andn2, wqm) = if wave64 {
        (0x04, 0x15, 0x0A)
    } else {
        (0x03, 0x14, 0x09)
    };
    let mut w = vec![
        sop1(0x03, 5, 0x80),                    // s_mov_b32 s5, 0 (table address, high)
        sop1(0x03, 7, 0x80),                    // s_mov_b32 s7, 0
        0x3D << 26 | 3 << 18 | 8 << 6 | 4 >> 1, // s_load_dwordx8 s[8:15], s[4:5], 0
        125 << 25,
        0x3D << 26 | 2 << 18 | 16 << 6 | 6 >> 1, // s_load_dwordx4 s[16:19], s[6:7], 0
        125 << 25,
        vop2(0x08, 2, 0xFF, 0), // v_mul_f32 v2, A, v0
        A.to_bits(),
        vop2(0x08, 3, 0xFF, 1), // v_mul_f32 v3, B, v1
        B.to_bits(),
    ];
    let mut kill_at = None;
    if discard {
        w.extend([
            vop1(0x07, 6, v(0)),                           // v_cvt_u32_f32 v6, v0 (x)
            vop1(0x07, 7, v(1)),                           // v_cvt_u32_f32 v7, v1 (y)
            vop2(0x0B, 8, 128 + 7, 6),                     // v_mul_u32_u24 v8, 7, v6
            vop2(0x0B, 9, 128 + 3, 7),                     // v_mul_u32_u24 v9, 3, v7
            vop2(0x25, 8, v(8), 9),                        // v_add_nc_u32 v8, v8, v9
            vop2(0x1B, 8, 128 + 5, 8),                     // v_and_b32 v8, 5, v8
            sop1(mov, 20, EXEC_LO),                        // s_mov s20, exec (live)
            0x7C00_0000 | 0xD2 << 17 | 8 << 9 | (128 + 5), // v_cmpx_eq_u32 5, v8 (killed)
            sop2(andn2, 20, 20, EXEC_LO),                  // s_andn2 s20, s20, exec
        ]);
        kill_at = Some(w.len());
        w.push(0); // s_cbranch_scc0 -> kill, patched below
        w.push(sop1(wqm, EXEC_LO, 20)); // s_wqm exec, s20
    }
    w.extend([
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        // image_get_lod v[4:5], v[2:3], s[8:15], s[16:19] dmask:0x3 dim:2D
        0xF000_0308 | 0x60 << 18,
        2 | 4 << 8 | 2 << 16 | 4 << 21,
        0xBF8C_3F70,         // s_waitcnt vmcnt(0)
        vop1(0x01, 6, 0xF2), // v_mov_b32 v6, 1.0
    ]);
    if discard {
        w.push(sop1(mov, EXEC_LO, 20)); // s_mov exec, s20: the live pixels export
    }
    w.extend([0xF800_180F, 0x0606_0504, S_ENDPGM]); // exp mrt0 v4, v5, v6, v6 done vm
    if let Some(at) = kill_at {
        let offset = (w.len() - at - 1) as u32;
        w[at] = 0xBF80_0000 | 0x04 << 16 | offset; // s_cbranch_scc0
        w.extend([0xF800_1890, 0, S_ENDPGM]); // kill: exp null done vm
    }
    w
}

fn sig(name: &str, kind: u8, interp: u8) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0xF,
        half: false,
    }
}

fn translate(native: &NativeProgram, inputs: &[SigElement], outputs: &[SigElement]) -> HostShader {
    let isg = signature(inputs, true);
    let osg = signature(outputs, false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg), (FourCC::OSG1, &osg)]);
    let c = Container::parse(&bytes).expect("container");
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).expect("ir");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource");
    let io = io::raise(&p, native).expect("io");
    let h = emit(&c, native, &p, &st, &io, &res).expect("emit");
    if let Ok(mut val) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        val.validate(&mut h.dxil.clone())
            .expect("DXIL validation failed");
    }
    h
}

fn param(root: RootType, user_data: u8, slot: u8) -> RootParam {
    RootParam {
        root,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    }
}

/// `image_get_lod` at every drawn pixel gives the level of the coordinates'
/// derivatives (clamped to the texture's one level: 0), next to a triangle
/// edge and next to pixels a discard killed, in wave32 and wave64, on the
/// hardware adapter and WARP. Tolerance 0.02 of a level, as `diff_tests`
/// takes for the hardware's 1/256 level steps.
#[test]
fn levels_of_detail_at_a_triangle_edge_and_after_a_discard() {
    let vs_code = [0xF800_08CF, 0x0706_0504, S_ENDPGM]; // exp pos0 v4..v7 done
    let vs = translate(
        &NativeProgram {
            stage: HwStage::Vs,
            wave64: true,
            vgprs: 8,
            layout: Vec::new(),
            used: Vec::new(),
            io: StageIo::Vs {
                inputs: vec![VsInput {
                    usage: Usage::Generic,
                    row: 0,
                    mask: 0xF,
                    vgpr: 4,
                }],
                outputs: Vec::new(),
            },
            code: vs_code.iter().flat_map(|w| w.to_le_bytes()).collect(),
        },
        &[sig("POSITION", 0, 0)],
        &[sig("SV_Position", 3, 4)],
    );
    // A triangle whose diagonal crosses quads of a 16x16 target.
    let vertices = [
        -1.0, -1.0, 0.5, 1.0, //
        -1.0, 1.1, 0.5, 1.0, //
        1.1, -1.0, 0.5, 1.0,
    ];
    let texels = vec![[0.5f32; 4]; (SIZE * SIZE) as usize];
    let linear = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        MaxAnisotropy: 1,
        MaxLOD: f32::MAX,
        ..Default::default()
    };
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let hosts: Vec<(&str, Host)> = [("hardware", false), ("warp", true)]
        .into_iter()
        .filter_map(|(label, warp)| Host::new(warp).ok().map(|h| (label, h)))
        .collect();
    assert!(!hosts.is_empty(), "no SM 6.6 device");
    let want = level();
    let mut failures = Vec::new();
    for discard in [false, true] {
        for wave64 in [false, true] {
            let ps = translate(
                &NativeProgram {
                    stage: HwStage::Ps,
                    wave64,
                    vgprs: 12,
                    layout: vec![
                        param(RootType::Constants, 0, 0),
                        param(RootType::Table, 4, 1),
                        param(RootType::Table, 6, 2),
                    ],
                    used: [(4, ResourceType::Srv, 8), (6, ResourceType::Sampler, 4)]
                        .map(|(user_data, kind, dwords)| UsedResource {
                            kind,
                            root: RootType::Table,
                            table_offset_dwords: 0,
                            lower_bound: 0,
                            stride: 0,
                            dwords,
                            user_data,
                            slot: user_data / 2 - 1,
                        })
                        .to_vec(),
                    io: StageIo::Ps {
                        inputs: Vec::new(),
                        input_addr: 0x300, // SV_Position.x in v0, .y in v1
                        db_shader_control: 0,
                        patches: Vec::new(),
                    },
                    code: pixel_program(wave64, discard)
                        .iter()
                        .flat_map(|w| w.to_le_bytes())
                        .collect(),
                },
                &[sig("SV_Position", 3, 4)],
                &[sig("SV_Target", 16, 0)],
            );
            // The tables are heap index 0 of each heap.
            let user_data = vec![0u32; ps.user_data_dwords().max(1) as usize];
            for (adapter, host) in &hosts {
                let label = format!(
                    "{} wave{} {adapter}",
                    if discard { "discard" } else { "edge" },
                    if wave64 { 64 } else { 32 }
                );
                let (root, pso) = host
                    .pipeline(&vs, &ps)
                    .unwrap_or_else(|e| panic!("{label}: pipeline: {e}"));
                let pixels = host
                    .draw(
                        &root,
                        &pso,
                        &Draw {
                            roots: vec![(1, Root::Constants(user_data.clone()))],
                            textures: &[(SIZE, SIZE, &texels)],
                            samplers: &[linear],
                            vertices: &vertices,
                            stride: 16,
                            size: (16, 16),
                            mesh_groups: 0,
                        },
                    )
                    .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
                let (mut drawn, mut wrong, mut first) = (0, 0, None);
                let (mut lo, mut hi) = (f32::MAX, f32::MIN);
                for (i, px) in pixels.iter().enumerate() {
                    if px[3] != 1.0 {
                        continue;
                    }
                    drawn += 1;
                    let (x, y) = (i as u32 % 16, i as u32 / 16);
                    lo = lo.min(px[1]);
                    hi = hi.max(px[1]);
                    let bad = discard && killed(x, y)
                        || !px[0].is_finite()
                        || !px[1].is_finite()
                        || px[0].abs() > 0.02
                        || (px[1] - want).abs() > 0.02;
                    if bad {
                        wrong += 1;
                        if first.is_none() {
                            first = Some(format!("({x}, {y}): [{:.4}, {:.4}]", px[0], px[1]));
                        }
                    }
                }
                eprintln!(
                    "{label}: {drawn} pixels, {wrong} wrong (want [0, {want:.4}], unclamped {lo:.4}..{hi:.4}){}",
                    first
                        .as_deref()
                        .map(|f| format!(", first {f}"))
                        .unwrap_or_default()
                );
                if drawn < 50 || wrong > 0 {
                    failures.push(format!("{label}: {wrong} of {drawn}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
