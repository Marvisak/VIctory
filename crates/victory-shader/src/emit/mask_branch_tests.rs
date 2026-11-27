//! A branch on the SCC of a mask operation nothing else reads (`s_and
//! sX, vcc, exec` then `s_cbranch_scc0/1`) is the console wave's decision:
//! "some lane of the mask is set", the same arm for every active lane,
//! whatever the width of the host's waves. Programs by [`super::mask_branch_repro`].

use super::host::{Dispatch, Host, Root, View};
use super::mask_branch_repro::{Program, program};
use crate::native::RootType;

const KINDS: [Program; 4] = [
    Program::Branch,
    Program::BranchScc1,
    Program::StoreOnScc0,
    Program::StoreOnScc1,
];

fn emit(kind: Program, lanes: u32) -> super::HostShader {
    let n = program(kind, lanes);
    let (p, st) = crate::range::build_native(&n).unwrap();
    let st = st.unwrap();
    assert_eq!(st.group_votes.len(), 2, "{kind:?}, {lanes} lanes");
    let res = crate::resource::track(&p, &n.layout, n.stage, Some(&st)).unwrap();
    let io = crate::io::raise(&p, &n).unwrap();
    let bytes = crate::dxbc::build(&[]);
    let c = crate::Container::parse(&bytes).unwrap();
    let shader = super::emit(&c, &n, &p, &st, &io, &res).unwrap();
    // The group's vote needs no wave size: a host whose waves are 32 lanes
    // must be able to create the Wave64 program's pipeline too.
    assert_eq!(
        required_wave_size(&shader.dxil),
        None,
        "{kind:?}, {lanes} lanes"
    );
    shader
}

/// The wave size the container's PSV0 requires (`WaveSize`), if any.
fn required_wave_size(dxil: &[u8]) -> Option<u32> {
    let word = |at: usize| u32::from_le_bytes(dxil[at..at + 4].try_into().unwrap()) as usize;
    (0..word(28))
        .map(|k| word(32 + 4 * k))
        .find(|&at| &dxil[at..at + 4] == b"PSV0")
        .and_then(|at| {
            // Part header (name, size), the info size, then PSInfo: the
            // minimum and maximum wave lane counts at bytes 16 and 20.
            let (min, max) = (word(at + 8 + 4 + 16) as u32, word(at + 8 + 4 + 20) as u32);
            (min != 0 || max != u32::MAX).then_some(min)
        })
}

/// The branch's SCC is found, and only for a mask nothing else reads.
#[test]
fn only_a_branch_reads_the_scc_of_a_mask_it_is_found() {
    for lanes in [32, 64] {
        for kind in KINDS.into_iter().chain([Program::BranchMaskRead]) {
            let n = program(kind, lanes);
            let (p, st) = crate::range::build(&n.patched_code(), n.wave64, n.vgprs).unwrap();
            let st = st.unwrap();
            let want = usize::from(!matches!(kind, Program::BranchMaskRead));
            assert_eq!(st.any_sccs.len(), want, "{kind:?}, {lanes} lanes");
            drop(p);
            if matches!(kind, Program::BranchMaskRead) {
                let (_, grouped) = crate::range::build_native(&n).unwrap();
                assert!(grouped.unwrap().group_votes.is_empty(), "{lanes} lanes");
            }
        }
    }
}

/// The shaders with the wave's vote validate.
#[test]
fn the_branches_on_the_waves_vote_validate() {
    for lanes in [32, 64] {
        for kind in KINDS {
            let shader = emit(kind, lanes);
            if let Ok(mut v) = super::validator::Validator::load(
                super::validator::Validator::default_path().as_deref(),
            ) {
                v.validate(&mut shader.dxil.clone())
                    .unwrap_or_else(|e| panic!("{kind:?}, {lanes} lanes: {e}"));
            }
        }
    }
}

/// Every active lane of the group takes the arm the wave's mask decides, in
/// groups of the console's two wave widths, all lanes or only some active;
/// the lanes outside keep their zero.
#[test]
fn every_active_lane_takes_the_arm_of_the_waves_vote_on_gpu() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    // (lanes below the count run, the lane whose compare matches)
    let cases = |lanes: u32| -> Vec<(u32, u32)> {
        if lanes == 32 {
            vec![
                (0, 0),
                (1, 0),
                (16, 15),
                (16, 16),
                (24, 7),
                (24, 21),
                (24, 25),
                (32, 31),
                (32, 32),
            ]
        } else {
            // 48 active: lane 37 matches, or lane 49 or 60 (outside the
            // running lanes), or the last running lane.
            vec![
                (0, 0),
                (1, 0),
                (32, 31),
                (32, 32),
                (48, 7),
                (48, 37),
                (48, 47),
                (48, 49),
                (48, 60),
                (64, 63),
                (64, 64),
            ]
        }
    };
    let mut ran = 0;
    for lanes in [32, 64] {
        for kind in KINDS {
            let shader = emit(kind, lanes);
            let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
            for (below, selected) in cases(lanes) {
                let mut data = vec![0; shader.user_data_dwords().max(1) as usize];
                for u in &shader.user_data {
                    if u.root == RootType::Constants && u.slot == 0 && u.dword < 2 {
                        data[u.index as usize] = [below, selected][u.dword as usize];
                    }
                }
                let want: Vec<u32> = (0..lanes)
                    .map(|lane| {
                        if lane < below {
                            1 + u32::from(selected < below)
                        } else {
                            0
                        }
                    })
                    .collect();
                let got = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(0, Root::Constants(data))],
                            views: &[View::RawUav(lanes)],
                            samplers: &[],
                            groups: [1, 1, 1],
                        },
                    )
                    .expect("dispatch")
                    .remove(0);
                assert_eq!(
                    got, want,
                    "{kind:?}, {lanes} lanes, {below} active, lane {selected}"
                );
                ran += 1;
            }
        }
    }
    eprintln!("{ran} dispatches");
}

/// Beyond one console wave there is no group vote: the branch is the
/// console wave's own, so the host must run waves of that width.
#[test]
fn a_group_of_several_console_waves_keeps_the_native_wave_size() {
    use crate::native::StageIo;
    for lanes in [32u32, 64] {
        let mut n = program(Program::Branch, lanes);
        let StageIo::Cs(io) = &mut n.io else {
            unreachable!()
        };
        io.threads[0] *= 2;
        let (p, st) = crate::range::build_native(&n).unwrap();
        let st = st.unwrap();
        let res = crate::resource::track(&p, &n.layout, n.stage, Some(&st)).unwrap();
        let io = crate::io::raise(&p, &n).unwrap();
        let bytes = crate::dxbc::build(&[]);
        let c = crate::Container::parse(&bytes).unwrap();
        let s = super::emit(&c, &n, &p, &st, &io, &res).unwrap();
        assert_eq!(required_wave_size(&s.dxil), Some(lanes));
    }
}
