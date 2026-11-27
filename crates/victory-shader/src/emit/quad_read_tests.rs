//! Quad reads at a triangle edge, without the console's compiler: the
//! cases `flow_tests` draws from console-compiled HLSL (which needs the
//! console SDK's `dxc.exe`, so most machines skip them), hand-assembled.
//! Each pixel computes a key from its position and reads its x, y and
//! diagonal neighbours' keys with DPP `quad_perm`, as the console compiler
//! takes derivatives and `QuadReadAcross*`. Along the edge some neighbours
//! are helper lanes (whole-quad mode), and with a discard some are killed
//! pixels, which stay helpers: either must still give its key.

use super::container::{SigElement, signature};
use super::host::{Draw, Host};
use super::{HostShader, emit};
use crate::native::{HwStage, NativeProgram, StageIo, Usage, VsInput};
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
/// `v_mov_b32 v<d>, v<s> quad_perm:[..] bound_ctrl:1`.
const fn quad_perm(d: u32, s: u32, ctrl: u32) -> [u32; 2] {
    [
        0x7E00_0000 | d << 17 | 0x01 << 9 | 0xFA,
        0xF << 28 | 0xF << 24 | 1 << 19 | ctrl << 8 | s,
    ]
}

/// Each pixel's key, `3x + 5y + 1`: distinct within a quad.
fn key(x: u32, y: u32) -> u32 {
    3 * x + 5 * y + 1
}

/// Whether the discarding program kills pixel (x, y).
fn killed(x: u32, y: u32) -> bool {
    (7 * x + 3 * y) & 5 == 5
}

/// The key in v4, optionally a discard of the pixels `killed` names (the
/// console's form: `v_cmpx` of the lanes killed, `s_andn2` of the live
/// mask and its SCC to a null export, then EXEC the whole quads of the
/// live mask), then the x, y and diagonal neighbours' keys in v5-v7.
/// Exports (key, x neighbour, y neighbour, diagonal) as floats, from the
/// live pixels.
fn pixel_program(wave64: bool, discard: bool) -> Vec<u32> {
    // s_mov, s_andn2 and s_wqm on EXEC's 32 or 64 bits.
    let (mov, andn2, wqm) = if wave64 {
        (0x04, 0x15, 0x0A)
    } else {
        (0x03, 0x14, 0x09)
    };
    let mut w = vec![
        vop1(0x07, 2, v(0)),       // v_cvt_u32_f32 v2, v0 (x)
        vop1(0x07, 3, v(1)),       // v_cvt_u32_f32 v3, v1 (y)
        vop2(0x0B, 4, 128 + 3, 2), // v_mul_u32_u24 v4, 3, v2
        vop2(0x0B, 5, 128 + 5, 3), // v_mul_u32_u24 v5, 5, v3
        vop2(0x25, 4, v(4), 5),    // v_add_nc_u32 v4, v4, v5
        vop2(0x25, 4, 128 + 1, 4), // v_add_nc_u32 v4, 1, v4
    ];
    let mut kill_at = None;
    if discard {
        w.extend([
            vop2(0x0B, 8, 128 + 7, 2),                     // v_mul_u32_u24 v8, 7, v2
            vop2(0x0B, 9, 128 + 3, 3),                     // v_mul_u32_u24 v9, 3, v3
            vop2(0x25, 8, v(8), 9),                        // v_add_nc_u32 v8, v8, v9
            vop2(0x1B, 8, 128 + 5, 8),                     // v_and_b32 v8, 5, v8
            sop1(mov, 10, EXEC_LO),                        // s_mov s10, exec (live)
            0x7C00_0000 | 0xD2 << 17 | 8 << 9 | (128 + 5), // v_cmpx_eq_u32 5, v8 (killed)
            sop2(andn2, 10, 10, EXEC_LO),                  // s_andn2 s10, s10, exec
        ]);
        kill_at = Some(w.len());
        w.push(0); // s_cbranch_scc0 -> kill, patched below
        w.push(sop1(wqm, EXEC_LO, 10)); // s_wqm exec, s10
    }
    for (d, ctrl) in [(5, 0xB1), (6, 0x4E), (7, 0x1B)] {
        w.extend(quad_perm(d, 4, ctrl)); // [1,0,3,2], [2,3,0,1], [3,2,1,0]
    }
    for r in 4..8 {
        w.push(vop1(0x06, r, v(r))); // v_cvt_f32_u32
    }
    if discard {
        w.push(sop1(mov, EXEC_LO, 10)); // s_mov exec, s10: the live pixels export
    }
    w.extend([0xF800_180F, 0x0706_0504, S_ENDPGM]); // exp mrt0 v4-v7 done vm
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

/// Quad reads of computed values reach the neighbours a triangle edge
/// leaves outside it (helper lanes) and the pixels a discard kills: every
/// drawn pixel reads its x, y and diagonal neighbours' keys, in wave32 and
/// wave64, on the hardware adapter and WARP.
#[test]
fn quad_reads_at_a_triangle_edge_reach_helpers_and_killed_pixels() {
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
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let hosts: Vec<(&str, Host)> = [("hardware", false), ("warp", true)]
        .into_iter()
        .filter_map(|(label, warp)| Host::new(warp).ok().map(|h| (label, h)))
        .collect();
    assert!(!hosts.is_empty(), "no SM 6.6 device");
    let mut failures = Vec::new();
    for discard in [false, true] {
        for wave64 in [false, true] {
            let ps = translate(
                &NativeProgram {
                    stage: HwStage::Ps,
                    wave64,
                    vgprs: 10,
                    layout: Vec::new(),
                    used: Vec::new(),
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
                            roots: Vec::new(),
                            textures: &[],
                            samplers: &[],
                            vertices: &vertices,
                            stride: 16,
                            size: (16, 16),
                            mesh_groups: 0,
                        },
                    )
                    .unwrap_or_else(|e| panic!("{label}: draw: {e}"));
                let (mut drawn, mut wrong, mut first) = (0, 0, None);
                for (i, px) in pixels.iter().enumerate() {
                    let (x, y) = (i as u32 % 16, i as u32 / 16);
                    if px[0] != key(x, y) as f32 {
                        // Not drawn: outside the triangle or killed.
                        if discard && px[0] != 0.0 {
                            wrong += 1;
                        }
                        continue;
                    }
                    drawn += 1;
                    if discard && killed(x, y) {
                        wrong += 1; // a killed pixel was written
                        continue;
                    }
                    let want = [key(x ^ 1, y), key(x, y ^ 1), key(x ^ 1, y ^ 1)].map(|k| k as f32);
                    if px[1..] != want {
                        wrong += 1;
                        if first.is_none() {
                            first = Some(format!("({x}, {y}): {:?} instead of {want:?}", &px[1..]));
                        }
                    }
                }
                eprintln!(
                    "{label}: {drawn} pixels, {wrong} wrong{}",
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
