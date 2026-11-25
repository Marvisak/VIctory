#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Stats {
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

pub(super) fn summarize(mut samples: Vec<f64>) -> Option<Stats> {
    if samples.is_empty() || samples.iter().any(|x| !x.is_finite() || *x < 0.0) {
        return None;
    }
    samples.sort_by(f64::total_cmp);
    let n = samples.len();
    let median = if n.is_multiple_of(2) {
        samples[n / 2 - 1].midpoint(samples[n / 2])
    } else {
        samples[n / 2]
    };
    // Nearest rank: ceil(0.95 * n) = n - floor(n / 20).
    Some(Stats {
        median,
        p95: samples[n - n / 20 - 1],
        max: samples[n - 1],
    })
}

/// Relative change is undefined for zero baseline and a nonzero candidate.
/// Both zero is treated as no change.
pub(super) fn percent(before: f64, after: f64) -> Option<f64> {
    if before == 0.0 {
        return (after == 0.0).then_some(0.0);
    }
    let delta = ((after - before) / before) * 100.0;
    delta.is_finite().then_some(delta)
}
