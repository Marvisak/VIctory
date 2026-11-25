use super::super::{compare, coverage, render};

#[test]
fn only_aggregate_metrics_are_printed() {
    let before = format!(
        "secret local path and diagnostic example\n{}",
        include_str!("../fixtures/census-baseline.txt")
    );
    let a = coverage::parse(&before).unwrap();
    let b = coverage::parse(include_str!("../fixtures/census-improved.txt")).unwrap();
    let output = render::text(&compare::build(&a, &b, None, None));
    assert!(output.contains("Result: PASS"));
    assert!(output.contains("vertex/pixel"));
    assert!(!output.contains("secret local path"));
    assert!(output.contains("not proof of shader identity"));
}
