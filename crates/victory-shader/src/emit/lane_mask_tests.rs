//! Lane masks read as numbers in pixel programs: a loop that serves one
//! value per pass (`s_ff1` of EXEC, `v_readlane`, `v_cmpx`), the console
//! compiler's expansion of a wave operation over a per-lane value, and
//! `v_readfirstlane`, drawn on the host adapter and WARP in wave32 and
//! wave64.

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
/// as a waterfall over a per-lane value does. Exports (the key served, the
/// lane's key) twice, as floats.
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
        sop1(mov, 12, EXEC_LO),    // s_mov s12, exec
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
        vop1(0x06, 5, v(5)),    // v_cvt_f32_u32 v5, v5
        vop1(0x06, 4, v(4)),    // v_cvt_f32_u32 v4, v4
        0xF800_180F,            // exp mrt0 v5, v4, v5, v4 done vm
        0x0405_0405,
        S_ENDPGM,
    ]);
    w
}

/// Reads the first lane's `x + 16y + 1` (unique per pixel) with
/// `v_readfirstlane_b32`. Exports (the key read, the lane's key) twice, as
/// floats.
fn first_lane_program(_wave64: bool) -> Vec<u32> {
    vec![
        vop1(0x07, 2, v(0)),        // v_cvt_u32_f32 v2, v0 (x)
        vop1(0x07, 3, v(1)),        // v_cvt_u32_f32 v3, v1 (y)
        vop2(0x0B, 5, 128 + 16, 3), // v_mul_u32_u24 v5, 16, v3
        vop2(0x25, 4, v(2), 5),     // v_add_nc_u32 v4, v2, v5
        vop2(0x25, 4, 128 + 1, 4),  // v_add_nc_u32 v4, 1, v4
        vop1(0x02, 1, v(4)),        // v_readfirstlane_b32 s1, v4
        vop1(0x01, 5, 1),           // v_mov_b32 v5, s1
        vop1(0x06, 5, v(5)),        // v_cvt_f32_u32 v5, v5
        vop1(0x06, 4, v(4)),        // v_cvt_f32_u32 v4, v4
        0xF800_180F,                // exp mrt0 v5, v4, v5, v4 done vm
        0x0405_0405,
        S_ENDPGM,
    ]
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

/// Draws `program` (wave32, then wave64) from a full-screen triangle on
/// the hardware adapter and WARP: (label, console wave size, the 256
/// pixels) per draw. Panics without any SM 6.6 device.
fn draws(program: fn(bool) -> Vec<u32>) -> Vec<(String, usize, Vec<[f32; 4]>)> {
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
    // A triangle covering the whole 16x16 target: no helper lanes.
    let vertices = [
        -1.0, -1.0, 0.5, 1.0, //
        -1.0, 3.0, 0.5, 1.0, //
        3.0, -1.0, 0.5, 1.0,
    ];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for wave64 in [false, true] {
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
            code: program(wave64)
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
            out.push((label, if wave64 { 64 } else { 32 }, pixels));
        }
    }
    assert!(!out.is_empty(), "no SM 6.6 device");
    out
}

/// Every pixel is served its own key. Was a defect on hosts that run pixel
/// waves of 64 lanes (the Radeon 780M, RDNA3): a wave32 program's 32-bit
/// masks were the ballot's low dword in every lane, so lanes 32-63 tested
/// the bits of lanes 0-31 and were never served (88 of 256 pixels kept 0
/// there; none on WARP).
#[test]
fn wave32_lane_mask_loops_serve_every_lane_of_a_64_lane_host_wave() {
    let mut failures = Vec::new();
    for (label, _, pixels) in draws(pixel_program) {
        let mut wrong = 0;
        let mut first = None;
        for (i, px) in pixels.iter().enumerate() {
            let (x, y) = (i as u32 % 16, i as u32 / 16);
            let want = key(x, y) as f32;
            assert_eq!(px[1], want, "{label}: ({x}, {y}) not drawn");
            if px[0] != want {
                wrong += 1;
                if first.is_none() {
                    first = Some(format!("({x}, {y}): {} instead of {want}", px[0]));
                }
            }
        }
        eprintln!(
            "{label}: {wrong} of 256 pixels served a wrong key{}",
            first
                .as_deref()
                .map(|f| format!(", first {f}"))
                .unwrap_or_default()
        );
        if wrong > 0 {
            failures.push(format!("{label}: {wrong} of 256"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

/// `v_readfirstlane` gives each console wave its own first lane's value:
/// the pixels reading one value are at most a console wave, and include
/// the pixel whose value it is. Was a defect on hosts that run pixel waves
/// of 64 lanes (the Radeon 780M): `waveReadLaneFirst` over the host wave
/// gave lanes 32-63 of a wave32 program the first value of lanes 0-31.
#[test]
fn wave32_first_lane_reads_stay_in_the_console_wave_on_a_64_lane_host_wave() {
    let mut failures = Vec::new();
    for (label, wave, pixels) in draws(first_lane_program) {
        let mut groups = std::collections::BTreeMap::<u32, Vec<u32>>::new();
        for (i, px) in pixels.iter().enumerate() {
            let (x, y) = (i as u32 % 16, i as u32 / 16);
            let own = x + 16 * y + 1;
            assert_eq!(px[1], own as f32, "{label}: ({x}, {y}) not drawn");
            groups.entry(px[0] as u32).or_default().push(own);
        }
        let largest = groups.values().map(Vec::len).max().unwrap_or(0);
        let too_large = groups.values().filter(|g| g.len() > wave).count();
        let foreign = groups.iter().filter(|&(k, g)| !g.contains(k)).count();
        eprintln!(
            "{label}: {} values read, largest group {largest} pixels, {too_large} groups over {wave}, {foreign} not their own pixel's",
            groups.len()
        );
        if too_large > 0 || foreign > 0 {
            failures.push(format!(
                "{label}: largest {largest}, {too_large} over {wave}, {foreign} foreign"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
