//! A pixel program's lane-mask loop at a triangle edge: on the console the
//! helper lanes of whole-quad mode are in EXEC and are served like the
//! others, so quad reads of their results give the neighbour's value. On
//! the host, wave operations leave helper lanes out unless the program asks
//! for them (shader model 6.7's `WaveOpsIncludeHelperLanes`).

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

/// Each pixel's key, `3x + 5y + 1`: distinct within a quad.
fn key(x: u32, y: u32) -> u32 {
    3 * x + 5 * y + 1
}

/// Serves one key per pass (`s_ff1` of EXEC, `v_readlane`, `v_cmpx`),
/// as a waterfall over a per-lane value does, then reads the x neighbour's
/// result with DPP `quad_perm:[1,0,3,2]`. Exports (own result, neighbour's
/// result, own result, key) as floats.
fn pixel_program(wave64: bool) -> Vec<u32> {
    // s_mov, s_ff1_i32, s_andn2 on EXEC's 32 or 64 bits.
    let (mov, ff1, andn2) = if wave64 {
        (0x04, 0x14, 0x15)
    } else {
        (0x03, 0x13, 0x14)
    };
    let mut w = vec![
        vop1(0x07, 2, v(0)),       // v_cvt_u32_f32 v2, v0 (x)
        vop1(0x07, 3, v(1)),       // v_cvt_u32_f32 v3, v1 (y)
        vop2(0x0B, 4, 128 + 3, 2), // v_mul_u32_u24 v4, 3, v2
        vop2(0x0B, 5, 128 + 5, 3), // v_mul_u32_u24 v5, 5, v3
        vop2(0x25, 4, v(4), 5),    // v_add_nc_u32 v4, v4, v5
        vop2(0x25, 4, 128 + 1, 4), // v_add_nc_u32 v4, 1, v4
        vop1(0x01, 5, 128),        // v_mov_b32 v5, 0
        sop1(mov, 12, EXEC_LO),    // s_mov s12, exec (the whole quads)
        sop1(mov, 10, EXEC_LO),    // s_mov s10, exec (lanes left)
        sop1(ff1, 0, EXEC_LO),     // s_ff1_i32 s0, exec
        0xD760_0001,               // v_readlane_b32 s1, v4, s0
        0x0000_0104,
        0x7C00_0000 | 0xD2 << 17 | 4 << 9 | 1, // v_cmpx_eq_u32 s1, v4
        vop1(0x01, 5, 1),                      // v_mov_b32 v5, s1
        sop2(andn2, 10, 10, EXEC_LO),          // s_andn2 s10, s10, exec
        sop1(mov, EXEC_LO, 10),                // s_mov exec, s10
    ];
    w.push(0xBF80_0000 | 0x09 << 16 | (-8i32 as u32 & 0xFFFF)); // s_cbranch_execnz (s_ff1)
    w.extend([
        sop1(mov, EXEC_LO, 12), // s_mov exec, s12
        // v_mov_b32 v6, v5 quad_perm:[1,0,3,2] bound_ctrl:1
        0x7E00_0000 | 6 << 17 | 0x01 << 9 | 0xFA,
        0xF << 28 | 0xF << 24 | 1 << 19 | 0xB1 << 8 | 5,
        vop1(0x06, 5, v(5)), // v_cvt_f32_u32 v5, v5
        vop1(0x06, 6, v(6)), // v_cvt_f32_u32 v6, v6
        vop1(0x06, 4, v(4)), // v_cvt_f32_u32 v4, v4
        0xF800_180F,         // exp mrt0 v5, v6, v5, v4 done vm
        0x0405_0605,
        S_ENDPGM,
    ]);
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

/// The keys a lane-mask loop serves reach the helper lanes at a triangle
/// edge, so a quad read of a helper's result gives its key, as on the
/// console. Needs a host with shader model 6.7 (wave operations over helper
/// lanes). Was a defect: the host's ballots left helpers out, so a helper
/// was never served and its neighbour read 0 (8 of 136 pixels on a Radeon
/// 780M, RDNA3, and 18 on WARP).
#[test]
fn lane_mask_loops_serve_helper_lanes_at_a_triangle_edge() {
    // A host first: translation may depend on what the device supports.
    let _ = Host::new(false);
    let vs_code = [0xF800_08CF, 0x0706_0504, S_ENDPGM]; // exp pos0 v4..v7 done
    let vs_native = NativeProgram {
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
    };
    let vs = translate(
        &vs_native,
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
    let mut failures = Vec::new();
    let mut ran = false;
    {
        let wave64 = true;
        let ps_native = NativeProgram {
            stage: HwStage::Ps,
            wave64,
            vgprs: 8,
            layout: Vec::new(),
            used: Vec::new(),
            io: StageIo::Ps {
                inputs: Vec::new(),
                input_addr: 0x300, // SV_Position.x in v0, .y in v1
                db_shader_control: 0,
                patches: Vec::new(),
            },
            code: pixel_program(wave64)
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect(),
        };
        let ps = translate(
            &ps_native,
            &[sig("SV_Position", 3, 4)],
            &[sig("SV_Target", 16, 0)],
        );
        for (adapter, warp) in [("hardware", false), ("warp", true)] {
            let label = format!("wave{} {adapter}", if wave64 { 64 } else { 32 });
            let Ok(host) = Host::new(warp) else {
                eprintln!("no SM 6.6 {label} device; skipped");
                continue;
            };
            ran = true;
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
            let (mut covered, mut own, mut neighbour) = (0, 0, 0);
            let (mut first, mut first_own) = (None, None);
            for (i, px) in pixels.iter().enumerate() {
                let (x, y) = (i as u32 % 16, i as u32 / 16);
                if px[3] != key(x, y) as f32 {
                    continue; // not covered
                }
                covered += 1;
                let want = [key(x, y) as f32, key(x ^ 1, y) as f32];
                if px[0] != want[0] {
                    own += 1;
                    if first_own.is_none() {
                        first_own =
                            Some(format!("({x}, {y}): own {} instead of {}", px[0], want[0]));
                    }
                }
                if px[1] != want[1] {
                    neighbour += 1;
                    if first.is_none() {
                        first = Some(format!(
                            "({x}, {y}): neighbour {} instead of {}",
                            px[1], want[1]
                        ));
                    }
                }
            }
            eprintln!(
                "{label}: {covered} pixels, own result wrong {own}, neighbour's wrong {neighbour}{}{}",
                first_own
                    .as_deref()
                    .map(|f| format!(", first {f}"))
                    .unwrap_or_default(),
                first
                    .as_deref()
                    .map(|f| format!(", first {f}"))
                    .unwrap_or_default()
            );
            assert!(covered > 100, "{label}: only {covered} pixels covered");
            if own + neighbour > 0 {
                failures.push(format!(
                    "{label}: own {own}, neighbour {neighbour} of {covered}"
                ));
            }
        }
    }
    assert!(ran, "no SM 6.6 device");
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
