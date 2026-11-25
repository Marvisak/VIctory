use super::super::coverage::{parse, Stage};

const BASE: &str = include_str!("../fixtures/census-baseline.txt");

#[test]
fn reads_all_stages() {
    let result = parse(BASE).unwrap();
    assert_eq!(result.get(Stage::Graphics).emitted, 90);
    assert_eq!(result.get(Stage::Compute).total, 20);
    assert_eq!(result.get(Stage::Mesh).pipelines, 8);
}

#[test]
fn rejects_truncated_and_invalid_censuses() {
    assert!(parse(include_str!("../fixtures/census-truncated.txt")).is_err());
    assert!(parse(include_str!("../fixtures/census-invalid.txt")).is_err());
    assert!(parse(&[BASE, BASE].concat()).is_err());
    assert!(parse(&BASE.replace("90 emitted", "101 emitted")).is_err());
}

#[test]
fn accepts_windows_line_endings() {
    assert!(parse(&BASE.replace('\n', "\r\n")).is_ok());
}
