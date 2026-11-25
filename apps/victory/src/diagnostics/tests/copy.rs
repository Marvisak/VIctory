use super::super::copy::{parse, Metric};

#[test]
fn extracts_complete_windows() {
    let p = parse(include_str!("../fixtures/copy-baseline.txt")).unwrap();
    assert_eq!(p.len(), 3);
    assert_eq!(p.values(Metric::TitleCopyMb), vec![20.0, 30.0, 40.0]);
    assert_eq!(p.values(Metric::HardFaults), vec![2.0, 0.0, 1.0]);
}

#[test]
fn refuses_missing_or_malformed_windows() {
    assert!(parse(include_str!("../fixtures/copy-invalid.txt")).is_err());
    assert!(parse(include_str!("../fixtures/copy-empty.txt")).is_err());
    let base = include_str!("../fixtures/copy-baseline.txt");
    assert!(parse(&base.replace("20.0 MB", "NaN MB")).is_err());
    assert!(parse(&base.replace("20.0 MB", "20.0 KB")).is_err());
}
