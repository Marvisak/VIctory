//! Which texels a gather (`image_gather4_lz`) and a point sample
//! (`image_sample_lz`) take near a texel boundary, without the console's
//! compiler. Each pixel looks up at `t = x + d` texels in u and v (a
//! gather's footprint and a point sample's texel start at `floor(t)`): away
//! from a boundary the texels are fixed; just below one (`d = -1/1024`) the
//! hardware's fixed-point coordinate decides whether `t` rounds up to `x`
//! or stays below it (`x - 1`), which hosts may do differently from the
//! console.

use windows::Win32::Graphics::Direct3D12::*;

use super::container::{SigElement, signature};
use super::host::{Draw, Host, Root};
use super::{HostShader, emit};
use crate::native::{
    HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, Usage, UsedResource,
    VsInput,
};
use crate::{Container, FourCC, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
const fn sop1(op: u32, d: u32, s: u32) -> u32 {
    0xBE80_0000 | d << 16 | op << 8 | s
}
const fn vop2(op: u32, d: u32, s0: u32, v1: u32) -> u32 {
    op << 25 | d << 17 | v1 << 9 | s0
}

/// The texture's size; its texel (i, j) holds `i + SIZE * j` in red.
const SIZE: u32 = 16;

/// Loads the descriptors as `lod_tests` does, puts `(x + 0.5 + d) / SIZE`
/// of the pixel position in v2 and v3 and exports what the MIMG `op` reads
/// with `dmask` into v4-v7.
fn pixel_program(d: f32, op: u32, dmask: u32) -> Vec<u32> {
    vec![
        sop1(0x03, 5, 0x80),                    // s_mov_b32 s5, 0 (table address, high)
        sop1(0x03, 7, 0x80),                    // s_mov_b32 s7, 0
        0x3D << 26 | 3 << 18 | 8 << 6 | 4 >> 1, // s_load_dwordx8 s[8:15], s[4:5], 0
        125 << 25,
        0x3D << 26 | 2 << 18 | 16 << 6 | 6 >> 1, // s_load_dwordx4 s[16:19], s[6:7], 0
        125 << 25,
        vop2(0x03, 2, 0xFF, 0), // v_add_f32 v2, d, v0
        d.to_bits(),
        vop2(0x03, 3, 0xFF, 1), // v_add_f32 v3, d, v1
        d.to_bits(),
        vop2(0x08, 2, 0xFF, 2), // v_mul_f32 v2, 1/SIZE, v2
        (1.0 / SIZE as f32).to_bits(),
        vop2(0x08, 3, 0xFF, 3), // v_mul_f32 v3, 1/SIZE, v3
        (1.0 / SIZE as f32).to_bits(),
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        // image_<op> v[4:7], v[2:3], s[8:15], s[16:19] dmask dim:2D
        0xF000_0008 | op << 18 | dmask << 8,
        2 | 4 << 8 | 2 << 16 | 4 << 21,
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xF800_180F, // exp mrt0 v4, v5, v6, v7 done vm
        0x0706_0504,
        S_ENDPGM,
    ]
}

/// The four red values a gather whose footprint starts at texel (i, j)
/// returns, in D3D's order: (i, j+1), (i+1, j+1), (i+1, j), (i, j); wrap.
fn gathered(i: i32, j: i32) -> [f32; 4] {
    let t = |i: i32, j: i32| {
        (i.rem_euclid(SIZE as i32) + SIZE as i32 * j.rem_euclid(SIZE as i32)) as f32
    };
    [t(i, j + 1), t(i + 1, j + 1), t(i + 1, j), t(i, j)]
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

/// Draws `op` (with `dmask`) at `t = x + d` for each `d`, from a full-screen
/// triangle on the hardware adapter and WARP. The program's coordinate is
/// `x + 0.5 + d + shift` texels: a gather's footprint starts half a texel
/// back from it (shift 0), a point sample's texel at it (shift -0.5). `exact` cases
/// must all take `value(floor(t))`; the others report whether each host
/// rounded `t` up to `x`. Returns the failures.
fn lookups(
    name: &str,
    op: u32,
    dmask: u32,
    shift: f32,
    value: fn(i32, i32) -> [f32; 4],
) -> Vec<String> {
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
    // A triangle covering the whole 16x16 target.
    let vertices = [
        -1.0, -1.0, 0.5, 1.0, //
        -1.0, 3.0, 0.5, 1.0, //
        3.0, -1.0, 0.5, 1.0,
    ];
    let texels: Vec<[f32; 4]> = (0..SIZE * SIZE)
        .map(|n| [n as f32, 0.0, 0.0, 1.0])
        .collect();
    let point = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
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
    let mut failures = Vec::new();
    for (d, exact) in [(0.25f32, true), (0.75, true), (-1.0 / 1024.0, false)] {
        let ps = translate(
            &NativeProgram {
                stage: HwStage::Ps,
                wave64: true,
                vgprs: 8,
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
                code: pixel_program(d + shift, op, dmask)
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .collect(),
            },
            &[sig("SV_Position", 3, 4)],
            &[sig("SV_Target", 16, 0)],
        );
        let user_data = vec![0u32; ps.user_data_dwords().max(1) as usize];
        for (adapter, host) in &hosts {
            let label = format!("{name} d {d:+.6} {adapter}");
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
                        samplers: &[point],
                        vertices: &vertices,
                        stride: 16,
                        size: (16, 16),
                        mesh_groups: 0,
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
            let (mut floor, mut rounded, mut other, mut first) = (0, 0, 0, None);
            for (n, px) in pixels.iter().enumerate() {
                let (x, y) = ((n as u32 % 16) as i32, (n as u32 / 16) as i32);
                let at = |k: i32| value(x + k, y + k);
                // At floor(t): x where t is inside texel x, x - 1 just below it.
                if *px == at(if exact { 0 } else { -1 }) {
                    floor += 1;
                } else if *px == at(0) {
                    rounded += 1;
                } else {
                    other += 1;
                    if first.is_none() {
                        first = Some(format!("({x}, {y}): {px:?}"));
                    }
                }
            }
            eprintln!(
                "{label}: 256 pixels, at floor(t) {floor}, rounded up to x {rounded}, neither {other}{}",
                first
                    .as_deref()
                    .map(|f| format!(", first {f}"))
                    .unwrap_or_default()
            );
            if other > 0 || exact && floor != 256 {
                failures.push(format!(
                    "{label}: floor {floor}, rounded {rounded}, neither {other}"
                ));
            }
        }
    }
    failures
}

/// `image_gather4_lz` takes the 2x2 footprint at `floor(t)` where `t` lies
/// a quarter texel inside a texel (`d` 0.25 and 0.75). Just below a
/// boundary (`d = -1/1024`) it reports which way each host rounds, without
/// failing: the console's hardware keeps 8 fractional bits.
#[test]
fn gathers_take_the_footprint_at_the_coordinate() {
    let failures = lookups("gather", 0x47, 0x1, 0.0, gathered);
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

/// `image_sample_lz` with a point sampler takes the texel at `floor(t)`
/// (`t = u * SIZE`), and reports the same boundary case.
#[test]
fn point_samples_take_the_texel_at_the_coordinate() {
    let texel = |i: i32, j: i32| {
        let n = i.rem_euclid(SIZE as i32) + SIZE as i32 * j.rem_euclid(SIZE as i32);
        [n as f32, 0.0, 0.0, 1.0]
    };
    let failures = lookups("point", 0x27, 0xF, -0.5, texel);
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
