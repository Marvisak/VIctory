use super::super::stats::{percent, summarize};

#[test]
fn median_and_nearest_rank_percentile() {
    let s = summarize(vec![40.0, 10.0, 30.0, 20.0]).unwrap();
    assert_eq!(s.median, 25.0);
    assert_eq!(s.p95, 40.0);
    let s = summarize((1..=20).map(f64::from).collect()).unwrap();
    assert_eq!(s.p95, 19.0);
    assert_eq!(s.max, 20.0);
}

#[test]
fn refuses_empty_and_nonfinite_data() {
    assert!(summarize(vec![]).is_none());
    assert!(summarize(vec![f64::NAN]).is_none());
    assert!(summarize(vec![-1.0]).is_none());
}

#[test]
fn percent_changes_have_defined_zero_behavior() {
    assert_eq!(percent(20.0, 10.0), Some(-50.0));
    assert_eq!(percent(0.0, 0.0), Some(0.0));
    assert_eq!(percent(0.0, 1.0), None);
}

#[test]
fn median_preserves_equal_extreme_finite_values() {
    for value in [f64::from_bits(1), f64::MIN_POSITIVE, f64::MAX] {
        let stats = summarize(vec![value, value]).unwrap();
        assert_eq!(stats.median, value);
        assert_eq!(stats.p95, value);
        assert_eq!(stats.max, value);
    }
}
