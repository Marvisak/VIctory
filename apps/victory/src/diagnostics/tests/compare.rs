use super::super::{compare::{build, Status}, coverage, copy};

fn baseline() -> coverage::Coverage {
    coverage::parse(include_str!("../fixtures/census-baseline.txt")).unwrap()
}

#[test]
fn gain_and_loss_are_not_net_averaged() {
    let a = baseline();
    let b = coverage::parse(include_str!("../fixtures/census-improved.txt")).unwrap();
    assert!(build(&a, &b, None, None).passed());
    let b = coverage::parse(include_str!("../fixtures/census-regression.txt")).unwrap();
    let report = build(&a, &b, None, None);
    assert!(!report.passed());
    assert_eq!(report.coverage[1].status, Status::Regressed);
}

#[test]
fn different_denominators_are_incomparable() {
    let a = baseline();
    let b = coverage::parse(include_str!("../fixtures/census-mismatch.txt")).unwrap();
    let report = build(&a, &b, None, None);
    assert!(!report.passed());
    assert_eq!(report.coverage[0].status, Status::Incomparable);
}

#[test]
fn explicit_copy_gate_can_fail_independently() {
    let a = baseline();
    let gpu = (
        copy::parse(include_str!("../fixtures/copy-candidate.txt")).unwrap(),
        copy::parse(include_str!("../fixtures/copy-baseline.txt")).unwrap(),
    );
    assert!(build(&a, &a, Some(&gpu), None).passed());
    assert!(!build(&a, &a, Some(&gpu), Some(10.0)).passed());
}
