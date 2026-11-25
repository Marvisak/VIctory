//! Offline comparisons of two translation censuses and optional GPU copy logs.
 //! No title is loaded, no GPU is opened, and the report writes only to stdout.
mod compare;
mod copy;
mod coverage;
mod input;
mod numbers;
mod render;
mod stats;

use std::path::Path;

pub(crate) fn run(
    baseline: &Path,
    candidate: &Path,
    baseline_gpu: Option<&Path>,
    candidate_gpu: Option<&Path>,
    max_copy_regression_percent: Option<f64>,
) -> anyhow::Result<()> {
    let threshold = max_copy_regression_percent;
    if let Some(n) = threshold {
        anyhow::ensure!(n.is_finite() && n >= 0.0, "copy threshold must be finite and nonnegative");
    }
    let read = |p: &Path, label: &str| {
        input::read(p).map_err(|e| anyhow::anyhow!("{label}: {e}"))
    };
    let before = coverage::parse(&read(baseline, "baseline census")?)
        .map_err(|e| anyhow::anyhow!("baseline census: {e}"))?;
    let after = coverage::parse(&read(candidate, "candidate census")?)
        .map_err(|e| anyhow::anyhow!("candidate census: {e}"))?;
    let gpu = match (baseline_gpu, candidate_gpu) {
        (Some(a), Some(b)) => {
            let a = copy::parse(&read(a, "baseline GPU log")?)
                .map_err(|e| anyhow::anyhow!("baseline GPU log: {e}"))?;
            let b = copy::parse(&read(b, "candidate GPU log")?)
                .map_err(|e| anyhow::anyhow!("candidate GPU log: {e}"))?;
            Some((a, b))
        }
        (None, None) => None,
        _ => anyhow::bail!("supply both GPU logs or neither"),
    };
    anyhow::ensure!(threshold.is_none() || gpu.is_some(), "copy threshold requires both GPU logs");
    let report = compare::build(&before, &after, gpu.as_ref(), threshold);
    print!("{}", render::text(&report));
    anyhow::ensure!(report.passed(), "diagnostic comparison did not pass");
    Ok(())
}

#[cfg(test)]
mod tests;
