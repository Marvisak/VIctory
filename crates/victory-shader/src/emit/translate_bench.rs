//! A stage-timed translation benchmark over a synthetic program, for
//! telling which pass a change makes faster or slower.

use super::container::SigElement;
use super::emit;
use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, StageIo};

fn output_signature() -> Container<'static> {
    let el = |name: &str, kind, comp| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp,
        interp: 0,
        row: None,
        col: 0,
        cols: 1,
        usage: 1,
        half: false,
    };
    let osg1 = crate::emit::container::signature(&[el("SV_Depth", 17, 9)], false);
    let bytes: &'static [u8] = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]).leak();
    Container::parse(bytes).unwrap()
}

/// A straight-line pixel program: v_mov_b32 v1, 5; s_mov_b32 s4, 0; then
/// 250 rounds of {40 `v_mad_f32` pairs (VOP3), s_addk_i32 s4, 1 (SOPK)},
/// exp mrtz, s_endpgm — 10,000 vector operations and no control flow, so
/// the stage times measure the straight-line passes only (structure
/// contributes what it costs to prove there is nothing to structure).
fn program() -> NativeProgram {
    let mut w = vec![0x7E02_0285, 0xBE84_0380];
    for _ in 0..250 {
        for k in 0..40 {
            w.push(0xD400_0000u32 | 0x141 << 16 | (k % 8) as u32); // v_mad_f32 v[k%8]
            w.push(128 | 128 << 9 | 128 << 18); // inline integer zeros
        }
        w.push(0xB784_0001); // s_addk_i32 s4, 1
    }
    w.push(0xF800_1883); // exp mrtz v0, v1, off, off compr done vm
    w.push(0x0000_0100);
    w.push(0xBF81_0000); // s_endpgm
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 16,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: w.iter().flat_map(|x| x.to_le_bytes()).collect(),
    }
}

/// The fixture is the straight-line workload its doc says: every
/// instruction decodes, under its own name, and makes one block.
#[test]
fn the_fixture_is_the_straight_line_workload_it_draws() {
    let native = program();
    let seen = crate::gfx10::decode(native.patched_code().as_slice()).unwrap();
    let names: Vec<&str> = seen.iter().filter_map(|i| i.name()).collect();
    assert_eq!(names.iter().filter(|n| **n == "v_mad_f32").count(), 10_000);
    assert_eq!(names.iter().filter(|n| **n == "s_addk_i32").count(), 250);
    assert!(names.iter().all(|n| !n.starts_with("s_cbranch") && *n != "s_branch"));
    let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    assert_eq!(p.blocks.len(), 1, "a straight-line program has one block");
}

/// Microseconds per stage, over at least 3 calls and 2 s.
#[test]
#[ignore = "benchmark: cargo test --release -p victory-shader bench_translate -- --ignored --nocapture"]
fn bench_translate() {
    let c = &output_signature();
    let native = program();
    let mut stages = [0f64; 5];
    let mut calls = 0;
    let t = std::time::Instant::now();
    while calls < 3 || t.elapsed().as_secs_f64() < 2.0 {
        let n = std::time::Instant::now();
        let p = crate::ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
        stages[0] += n.elapsed().as_secs_f64();
        let n = std::time::Instant::now();
        let st = crate::structure::structure(&p).unwrap();
        stages[1] += n.elapsed().as_secs_f64();
        let n = std::time::Instant::now();
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        stages[2] += n.elapsed().as_secs_f64();
        let n = std::time::Instant::now();
        let io = crate::io::raise(&p, &native).unwrap();
        stages[3] += n.elapsed().as_secs_f64();
        let n = std::time::Instant::now();
        let s = emit(c, &native, &p, &st, &io, &res).unwrap();
        stages[4] += n.elapsed().as_secs_f64();
        std::hint::black_box(&s);
        calls += 1;
    }
    let per = |x: f64| x / f64::from(calls) * 1e3;
    eprintln!(
        "calls={calls} ir={:.2}ms structure={:.2}ms resource={:.2}ms io={:.2}ms emit={:.2}ms",
        per(stages[0]),
        per(stages[1]),
        per(stages[2]),
        per(stages[3]),
        per(stages[4]),
    );
}
