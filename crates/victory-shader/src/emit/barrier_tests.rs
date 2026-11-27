//! Group barriers where lanes of a wave part (`lower/barriers.rs`): the
//! console runs `s_barrier` per wave, the host's barrier counts threads.
//! Synthetic GFX10 programs, one group of 64 threads in two waves of 32,
//! lowered, validated by `dxil.dll` when present, and run on the host GPU
//! and WARP. Each must complete (a group whose threads meet different
//! numbers of barriers hangs the GPU) and store what the console would:
//!
//! - a per-lane branch with a barrier on one side and one after the join
//!   (the lighter side gets as many barriers as it lacks);
//! - a loop holding a barrier, left per lane (its exit decided for the
//!   wave, as the console decides it);
//! - a per-lane side meeting barriers while the other meets none to the end
//!   (it exits; a host barrier does not wait for exited threads, which
//!   D3D12 leaves undefined: hence this test on both adapters);
//! - the same with a barrier after the join, whose counts cannot be
//!   balanced (console mode: branches decided for the wave, vector
//!   instructions guarded by EXEC).

use windows::Win32::Graphics::Direct3D12::{
    D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
};

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// Threads in the group: two waves of 32.
const THREADS: u32 = 64;

/// A program's words, with labels for branches.
#[derive(Default)]
struct Asm {
    words: Vec<u32>,
    /// (word of the branch, SOPP opcode, label).
    fixups: Vec<(usize, u32, &'static str)>,
    labels: Vec<(&'static str, usize)>,
}

impl Asm {
    fn put(&mut self, words: &[u32]) -> &mut Self {
        self.words.extend_from_slice(words);
        self
    }

    fn label(&mut self, name: &'static str) -> &mut Self {
        self.labels.push((name, self.words.len()));
        self
    }

    /// A SOPP branch (`op`) to `label`.
    fn branch(&mut self, op: u32, label: &'static str) -> &mut Self {
        self.fixups.push((self.words.len(), op, label));
        self.words.push(0);
        self
    }

    fn finish(&mut self) -> Vec<u32> {
        for &(at, op, label) in &self.fixups {
            let to = self.labels.iter().find(|l| l.0 == label).expect("label").1;
            // In dwords from the instruction after the branch.
            let rel = to as i64 - (at as i64 + 1);
            self.words[at] = 0xBF80_0000 | (op << 16) | (rel as i16 as u16 as u32);
        }
        std::mem::take(&mut self.words)
    }
}

const S_CBRANCH_SCC1: u32 = 0x05;
const S_CBRANCH_EXECZ: u32 = 0x08;
const S_CBRANCH_EXECNZ: u32 = 0x09;

/// The UAV (table entry 0, at s0) into s[40:43], v20 = thread × 4, v1 = 1.
const PROLOGUE: &[u32] = &[
    0xBE84_0300, // s_mov_b32 s4, s0
    0xBE85_0380, // s_mov_b32 s5, 0
    0xF408_0A02,
    0xFA00_0000, // s_load_dwordx4 s[40:43], s[4:5], 0x0
    0x3428_0082, // v_lshlrev_b32 v20, 2, v0
    0x7E02_0281, // v_mov_b32 v1, 1
    0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
];

/// v1 stored at v20, then the end.
const EPILOGUE: &[u32] = &[
    0xE070_1000,
    0x800A_0114, // buffer_store_dword v1, v20, s[40:43], 0 offen
    0xBF81_0000, // s_endpgm
];

const S_BARRIER: u32 = 0xBF8A_0000;
/// v_cmp_gt_u32 vcc_lo, 16, v0: threads 0-15 of the group.
const LANES_BELOW_16: u32 = 0x7D88_0090;
/// s_and_saveexec_b32 s6, vcc_lo
const SAVE_EXEC_VCC: u32 = 0xBE86_3C6A;
/// s_or_b32 exec_lo, exec_lo, s6
const RESTORE_EXEC: u32 = 0x887E_067E;
/// v_mov_b32 v1, 2
const V1_IS_2: u32 = 0x7E02_0282;
/// s_cmp_eq_u32 s0, 0: never true (s0 holds the table's address), but not
/// known when lowering: a uniform choice.
const S0_IS_0: u32 = 0xBF06_8000;

/// Threads 0-15 meet a barrier the others skip, and every thread one more
/// after the join: v1 = 2 for threads 0-15, else 1.
fn per_lane_branch() -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        .put(&[LANES_BELOW_16, SAVE_EXEC_VCC])
        .branch(S_CBRANCH_EXECZ, "join")
        .put(&[S_BARRIER, V1_IS_2])
        .label("join")
        .put(&[RESTORE_EXEC, S_BARRIER])
        .put(EPILOGUE);
    a.finish()
}

/// Each thread goes round (thread & 3) + 1 times, with a barrier in every
/// round, counting rounds in v1 (EXEC narrowed as threads finish).
fn loop_left_per_lane() -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        .put(&[
            0x3604_0083, // v_and_b32 v2, 3, v0
            0x4A04_0481, // v_add_nc_u32 v2, 1, v2
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE87_037E, // s_mov_b32 s7, exec_lo
        ])
        .label("round")
        .put(&[
            S_BARRIER,
            0x4A02_0281, // v_add_nc_u32 v1, 1, v1
            0x7D82_0501, // v_cmp_lt_u32 vcc_lo, v1, v2
            0x877E_6A7E, // s_and_b32 exec_lo, exec_lo, vcc_lo
        ])
        .branch(S_CBRANCH_EXECNZ, "round")
        .put(&[0xBEFE_0307]) // s_mov_b32 exec_lo, s7
        .put(EPILOGUE);
    a.finish()
}

/// Threads 0-15 meet one or two barriers (a uniform choice), the others none
/// to the end: v1 = 2 for threads 0-15, else 1. With `after`, every thread
/// meets one more after the join, so the sides' counts ({1, 2} against
/// {0}, then {2, 3} against {1}) differ by no one number.
fn uneven_sides(after: bool) -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        .put(&[LANES_BELOW_16, SAVE_EXEC_VCC])
        .branch(S_CBRANCH_EXECZ, "join")
        .put(&[S0_IS_0])
        .branch(S_CBRANCH_SCC1, "one")
        .put(&[S_BARRIER])
        .label("one")
        .put(&[S_BARRIER, V1_IS_2])
        .label("join")
        .put(&[RESTORE_EXEC]);
    if after {
        a.put(&[S_BARRIER]);
    }
    a.put(EPILOGUE);
    a.finish()
}

const S_CBRANCH_SCC0: u32 = 0x04;

/// Threads 0-15 go round a loop without barriers (thread & 3) + 1 times,
/// leaving it per lane, write 2 to LDS at their index and meet a barrier;
/// every thread meets one after the join and reads LDS at its index & 15:
/// 2 for every thread. The loop's latch must count the barriers after the
/// loop (its exit), not its back edge's none: counted as none, the side
/// looked lighter and got a barrier ahead of its write, which the other
/// threads' barrier paired with, so they read LDS before it was written.
fn loop_without_barriers_on_the_heavier_side() -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        // LDS keeps what an earlier dispatch left: every thread clears the
        // slot it reads first, and the group meets once.
        .put(&[
            0x7E0A_0280, // v_mov_b32 v5, 0
            0x360C_008F, // v_and_b32 v6, 15, v0
            0x340C_0C82, // v_lshlrev_b32 v6, 2, v6
            0xD834_0000,
            0x0000_0506, // ds_write_b32 v6, v5
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
            S_BARRIER,
        ])
        .put(&[LANES_BELOW_16, SAVE_EXEC_VCC])
        .branch(S_CBRANCH_EXECZ, "join")
        .put(&[
            0x3604_0083, // v_and_b32 v2, 3, v0
            0x4A04_0481, // v_add_nc_u32 v2, 1, v2
            0x7E06_0280, // v_mov_b32 v3, 0
            0xBE87_037E, // s_mov_b32 s7, exec_lo
        ])
        .label("round")
        .put(&[
            0x4A06_0681, // v_add_nc_u32 v3, 1, v3
            0x7D82_0503, // v_cmp_lt_u32 vcc_lo, v3, v2
            0x877E_6A7E, // s_and_b32 exec_lo, exec_lo, vcc_lo
        ])
        .branch(S_CBRANCH_EXECNZ, "round")
        .put(&[
            0xBEFE_0307, // s_mov_b32 exec_lo, s7
            V1_IS_2,
            0xD834_0000,
            0x0000_0114, // ds_write_b32 v20, v1
            S_BARRIER,
        ])
        .label("join")
        .put(&[
            RESTORE_EXEC,
            S_BARRIER,
            0x3608_008F, // v_and_b32 v4, 15, v0
            0x3408_0882, // v_lshlrev_b32 v4, 2, v4
            0xD8D8_0000,
            0x0100_0004, // ds_read_b32 v1, v4
            0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        ])
        .put(EPILOGUE);
    a.finish()
}

/// As [`loop_left_per_lane`], but the latch tests SCC of `s_cmp_eq_u32` of
/// EXEC with 0 (no lane left) and goes round while it is clear: SCC of an
/// equality of masks is "all lanes equal", so the wave goes round while any
/// lane is left.
fn loop_ended_by_a_mask_compare() -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        .put(&[
            0x3604_0083, // v_and_b32 v2, 3, v0
            0x4A04_0481, // v_add_nc_u32 v2, 1, v2
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE87_037E, // s_mov_b32 s7, exec_lo
        ])
        .label("round")
        .put(&[
            S_BARRIER,
            0x4A02_0281, // v_add_nc_u32 v1, 1, v1
            0x7D82_0501, // v_cmp_lt_u32 vcc_lo, v1, v2
            0x877E_6A7E, // s_and_b32 exec_lo, exec_lo, vcc_lo
            0xBF06_807E, // s_cmp_eq_u32 exec_lo, 0
        ])
        .branch(S_CBRANCH_SCC0, "round")
        .put(&[0xBEFE_0307]) // s_mov_b32 exec_lo, s7
        .put(EPILOGUE);
    a.finish()
}

const S_BRANCH: u32 = 0x02;

/// [`uneven_sides`]' barriers, which send the program to console mode, then
/// the console compiler's waterfall leaving when a counter equals each
/// lane's count: the count is `thread >> 4` (0 for threads 0-15, 1 to 3 for
/// the others, so the waves' largest differ). Lanes with 0 stay out
/// (`v_cmpx_eq_u32 0, v9`, then `s_andn2_b32 exec_lo`); s20 counts up, a
/// lane drops out when s20 equals its count, and the loop goes round while
/// any is left (`s_andn2_b32 s16, s16, exec_lo`, then SCC) through a block
/// that only narrows EXEC and goes back (`s_and_b32 exec_lo, s18, s16`).
/// Each pass adds 1 to v1: a lane stores its count. With `clobber`, the
/// body saves EXEC in s18, restores it, then uses s18 as a scalar before
/// saving EXEC there again.
fn equality_exit_waterfall(clobber: bool) -> Vec<u32> {
    let mut a = Asm::default();
    a.put(PROLOGUE)
        .put(&[LANES_BELOW_16, SAVE_EXEC_VCC])
        .branch(S_CBRANCH_EXECZ, "join")
        .put(&[S0_IS_0])
        .branch(S_CBRANCH_SCC1, "one")
        .put(&[S_BARRIER])
        .label("one")
        .put(&[S_BARRIER, V1_IS_2])
        .label("join")
        .put(&[RESTORE_EXEC, S_BARRIER])
        .put(&[
            0x2C12_0084, // v_lshrrev_b32 v9, 4, v0
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE8C_037E, // s_mov_b32 s12, exec_lo
            0x7DA4_1280, // v_cmpx_eq_u32 0, v9
            0x8A7E_7E0C, // s_andn2_b32 exec_lo, s12, exec_lo
        ])
        .branch(S_CBRANCH_EXECZ, "restore")
        .put(&[
            0xBE90_037E, // s_mov_b32 s16, exec_lo
            0xBE94_0380, // s_mov_b32 s20, 0
        ])
        .branch(S_BRANCH, "body")
        .label("top")
        .put(&[
            0x877E_1012, // s_and_b32 exec_lo, s18, s16
            0xBF80_0000, // s_nop 0
        ])
        .label("body");
    if clobber {
        a.put(&[
            0xBE92_037E, // s_mov_b32 s18, exec_lo
            0x4A02_0281, // v_add_nc_u32 v1, 1, v1
            0xBEFE_0312, // s_mov_b32 exec_lo, s18
            0x9312_8614, // s_mul_i32 s18, s20, 6
        ]);
    } else {
        a.put(&[0x4A02_0281]); // v_add_nc_u32 v1, 1, v1
    }
    a.put(&[
        0x8114_8114, // s_add_i32 s20, s20, 1
        0xBE92_037E, // s_mov_b32 s18, exec_lo
        0x7DA4_1214, // v_cmpx_eq_u32 s20, v9
        0xBF80_0000, // s_nop 0
        0x8A10_7E10, // s_andn2_b32 s16, s16, exec_lo
    ])
    .branch(S_CBRANCH_SCC1, "top")
    .label("restore")
    .put(&[0xBEFE_030C]) // s_mov_b32 exec_lo, s12
    .put(EPILOGUE);
    a.finish()
}

/// The barriers lowering adds on branch edges, by the name of the first
/// instruction of the block each edge leads into.
fn padded_edges(words: &[u32]) -> Vec<(&'static str, u32)> {
    let (lowered, firsts) = lowered(words);
    lowered
        .extra_barriers
        .iter()
        .map(|&((_, to), n)| (firsts[to], n))
        .collect()
}

/// Lowers `words`; with the name of each IR block's first instruction.
fn lowered(words: &[u32]) -> (lower::Lowered, Vec<&'static str>) {
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
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            // 512 bytes of LDS (`ComputeIo::lds_bytes`).
            rsrc2: 1 << 1 | 1 << 15,
            threads: [THREADS, 1, 1],
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
    let firsts = p.blocks.iter().map(|b| b.insts.first().map_or("", |i| i.name())).collect();
    (lowered, firsts)
}

fn build(words: &[u32]) -> HostShader {
    let (lowered, _) = lowered(words);
    HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        static_samplers: Vec::new(),
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
    }
}

/// The wave size `shader` requires (its `PSV0` minimum and maximum lane
/// counts, equal when set by `WaveSize`), if any.
fn required_wave(shader: &HostShader) -> Option<u32> {
    let c = crate::dxbc::Container::parse(&shader.dxil).ok()?;
    let d = c.part(crate::dxbc::FourCC::PSV0)?.data;
    let word = |at: usize| Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?));
    let (min, max) = (word(20)?, word(24)?);
    (min == max && min != 0).then_some(min)
}

/// The adapter's wave lane counts (`WaveLaneCountMin..=WaveLaneCountMax`).
fn lanes(host: &Host) -> (u32, u32) {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: a capability query on a live device, into a structure of the
    // size given.
    unsafe {
        host.device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut o).cast(),
            size_of_val(&o) as u32,
        )
    }
    .expect("OPTIONS1");
    (o.WaveLaneCountMin, o.WaveLaneCountMax)
}

/// Lowers, validates and runs `words` on the hardware adapter and WARP;
/// each must complete and store `want(thread)` per thread. An adapter
/// whose waves cannot be the size the program requires is skipped.
fn runs_as_the_console(name: &str, words: &[u32], want: impl Fn(u32) -> u32) {
    let shader = build(words);
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .unwrap_or_else(|e| panic!("{name}: DXIL validation failed: {e}"));
    }
    let views = [View::RawUav(THREADS)];
    let want: Vec<u32> = (0..THREADS).map(want).collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (mut ran, mut wave_skipped) = (0, false);
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        // A program that keeps the console's lanes requires their wave size
        // (`WaveSize`), which D3D12 refuses (`E_INVALIDARG`) on an adapter
        // whose waves are of another size: the system WARP's are 4 lanes.
        let (min, max) = lanes(&host);
        if let Some(wave) = required_wave(&shader)
            && !(min..=max).contains(&wave)
        {
            eprintln!(
                "{name}, {label}: needs {wave}-lane waves, the adapter runs {min}..{max}; skipped"
            );
            wave_skipped = true;
            continue;
        }
        let (root, pso) = host.compute_pipeline(&shader).expect("pipeline");
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
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
            .unwrap_or_else(|e| panic!("{name}, {label}: dispatch failed (a hang?): {e}"));
        assert_eq!(out[0][..THREADS as usize], want[..], "{name}, {label}");
        ran += 1;
    }
    // With WARP alone (CI), a program of the console's wave size runs nowhere.
    assert!(
        ran > 0 || wave_skipped,
        "neither hardware nor WARP SM 6.6 device available"
    );
}

/// The programs decode to the instructions they are meant to be.
#[test]
fn barrier_programs_decode_as_written() {
    let names = |words: Vec<u32>| -> Vec<&'static str> {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        crate::gfx10::decode(&code)
            .expect("decode")
            .iter()
            .map(|i| i.name().unwrap_or("?"))
            .collect()
    };
    let prologue = [
        "s_mov_b32",
        "s_mov_b32",
        "s_load_dwordx4",
        "v_lshlrev_b32",
        "v_mov_b32",
        "s_waitcnt",
    ];
    let epilogue = ["buffer_store_dword", "s_endpgm"];
    let with = |body: &[&'static str]| -> Vec<&'static str> {
        prologue.iter().chain(body).chain(&epilogue).copied().collect()
    };
    assert_eq!(
        names(per_lane_branch()),
        with(&[
            "v_cmp_gt_u32",
            "s_and_saveexec_b32",
            "s_cbranch_execz",
            "s_barrier",
            "v_mov_b32",
            "s_or_b32",
            "s_barrier",
        ])
    );
    assert_eq!(
        names(loop_left_per_lane()),
        with(&[
            "v_and_b32",
            "v_add_nc_u32",
            "v_mov_b32",
            "s_mov_b32",
            "s_barrier",
            "v_add_nc_u32",
            "v_cmp_lt_u32",
            "s_and_b32",
            "s_cbranch_execnz",
            "s_mov_b32",
        ])
    );
    let uneven = [
        "v_cmp_gt_u32",
        "s_and_saveexec_b32",
        "s_cbranch_execz",
        "s_cmp_eq_u32",
        "s_cbranch_scc1",
        "s_barrier",
        "s_barrier",
        "v_mov_b32",
        "s_or_b32",
    ];
    assert_eq!(names(uneven_sides(false)), with(&uneven));
    let after: Vec<&'static str> = uneven.iter().copied().chain(["s_barrier"]).collect();
    assert_eq!(names(uneven_sides(true)), with(&after));
    assert_eq!(
        names(loop_without_barriers_on_the_heavier_side()),
        with(&[
            "v_mov_b32",
            "v_and_b32",
            "v_lshlrev_b32",
            "ds_write_b32",
            "s_waitcnt",
            "s_barrier",
            "v_cmp_gt_u32",
            "s_and_saveexec_b32",
            "s_cbranch_execz",
            "v_and_b32",
            "v_add_nc_u32",
            "v_mov_b32",
            "s_mov_b32",
            "v_add_nc_u32",
            "v_cmp_lt_u32",
            "s_and_b32",
            "s_cbranch_execnz",
            "s_mov_b32",
            "v_mov_b32",
            "ds_write_b32",
            "s_barrier",
            "s_or_b32",
            "s_barrier",
            "v_and_b32",
            "v_lshlrev_b32",
            "ds_read_b32",
            "s_waitcnt",
        ])
    );
    assert_eq!(
        names(loop_ended_by_a_mask_compare()),
        with(&[
            "v_and_b32",
            "v_add_nc_u32",
            "v_mov_b32",
            "s_mov_b32",
            "s_barrier",
            "v_add_nc_u32",
            "v_cmp_lt_u32",
            "s_and_b32",
            "s_cmp_eq_u32",
            "s_cbranch_scc0",
            "s_mov_b32",
        ])
    );
    for clobber in [false, true] {
        let body: &[&str] = if clobber {
            &["s_mov_b32", "v_add_nc_u32", "s_mov_b32", "s_mul_i32"]
        } else {
            &["v_add_nc_u32"]
        };
        let waterfall: Vec<&'static str> = after
            .iter()
            .copied()
            .chain([
                "v_lshrrev_b32",
                "v_mov_b32",
                "s_mov_b32",
                "v_cmpx_eq_u32",
                "s_andn2_b32",
                "s_cbranch_execz",
                "s_mov_b32",
                "s_mov_b32",
                "s_branch",
                "s_and_b32",
                "s_nop",
            ])
            .chain(body.iter().copied())
            .chain([
                "s_add_i32",
                "s_mov_b32",
                "v_cmpx_eq_u32",
                "s_nop",
                "s_andn2_b32",
                "s_cbranch_scc1",
                "s_mov_b32",
            ])
            .collect();
        assert_eq!(names(equality_exit_waterfall(clobber)), with(&waterfall), "clobber {clobber}");
    }
}

#[test]
fn a_loop_without_barriers_counts_the_barriers_after_it() {
    runs_as_the_console(
        "loop without barriers on the heavier side",
        &loop_without_barriers_on_the_heavier_side(),
        |_| 2,
    );
}

#[test]
fn a_loop_ended_by_a_mask_compare_goes_round_while_any_lane_is_left() {
    runs_as_the_console("loop ended by a mask compare", &loop_ended_by_a_mask_compare(), |t| (t & 3) + 1);
}

#[test]
fn a_barrier_on_one_side_of_a_lane_branch_is_balanced() {
    runs_as_the_console("per-lane branch", &per_lane_branch(), |t| if t < 16 { 2 } else { 1 });
}

#[test]
fn a_loop_with_a_barrier_left_per_lane_ends_for_the_wave() {
    runs_as_the_console("loop left per lane", &loop_left_per_lane(), |t| (t & 3) + 1);
}

#[test]
fn a_side_meeting_no_more_barriers_exits() {
    runs_as_the_console("uneven sides, none after", &uneven_sides(false), |t| if t < 16 { 2 } else { 1 });
}

#[test]
fn barriers_that_cannot_be_balanced_run_as_the_console_runs_them() {
    runs_as_the_console("uneven sides, one after", &uneven_sides(true), |t| if t < 16 { 2 } else { 1 });
}

/// The side holding a loop without barriers, then a barrier, is the
/// heavier one: the padding goes on the edge of the side that skips it
/// (into the join, `s_or_b32` restoring EXEC), not into the loop's side.
/// Counting the loop's latch by its back edge (none) made the loop's side
/// look lighter and padded it instead; NVIDIA's results do not show that,
/// so this looks at the lowering.
#[test]
fn a_loop_without_barriers_pads_the_side_that_skips_it() {
    assert_eq!(padded_edges(&loop_without_barriers_on_the_heavier_side()), [("s_or_b32", 1)]);
}

/// In console mode, lanes the wave carries round a waterfall going round
/// through a block that only narrows EXEC are parked, and so are lanes it
/// carries into the loop: before, none was (the loop's only latch to the
/// header is that block's jump), so lanes already done, and threads 0-15,
/// which never enter, voted to go round and ran the body, and the loop
/// never ended (the iteration limit, no store).
#[test]
fn a_waterfall_going_round_through_a_jump_leaves_when_its_lanes_are_done() {
    runs_as_the_console("equality-exit waterfall", &equality_exit_waterfall(false), |t| t >> 4);
}

#[test]
fn a_waterfall_with_its_saved_exec_reused_as_a_scalar_leaves_too() {
    runs_as_the_console(
        "equality-exit waterfall, s18 reused",
        &equality_exit_waterfall(true),
        |t| t >> 4,
    );
}
