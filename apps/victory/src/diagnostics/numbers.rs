pub(super) fn count(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

pub(super) fn measure(text: &str) -> Option<f64> {
    let n: f64 = text.parse().ok()?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

pub(super) fn after(line: &str, marker: &str, unit: &str) -> Option<f64> {
    let (_, rest) = line.split_once(marker)?;
    let mut words = rest.split_whitespace();
    let n = measure(words.next()?)?;
    let actual = words.next()?.trim_end_matches([';', ',']);
    (actual == unit).then_some(n)
}

pub(super) fn before(line: &str, marker: &str) -> Option<f64> {
    let (prefix, _) = line.split_once(marker)?;
    measure(prefix.split_whitespace().last()?)
}
