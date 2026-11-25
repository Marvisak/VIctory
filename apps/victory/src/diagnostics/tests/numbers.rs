use super::super::numbers::*;

#[test]
fn counts_are_strict_and_checked() {
    assert_eq!(count(" 12 "), Some(12));
    for bad in ["", "-1", "+1", "1.5", "1,000", "18446744073709551616"] {
        assert_eq!(count(bad), None);
    }
}

#[test]
fn metrics_require_finite_nonnegative_values_and_correct_units() {
    for bad in ["NaN", "inf", "-1", "bad"] {
        assert_eq!(measure(bad), None);
    }
    assert_eq!(after("copies 3.5 MB;", "copies ", "MB"), Some(3.5));
    assert_eq!(after("copies 3.5 KB;", "copies ", "MB"), None);
    assert_eq!(before("10.0 submissions", " submissions"), Some(10.0));
}
