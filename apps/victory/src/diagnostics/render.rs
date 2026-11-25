use std::fmt::Write;
use super::compare::Report;

/// Deliberately print aggregate metrics only: no input paths, shader names,
/// raw log lines, or failure example strings.
pub(super) fn text(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "VIctory diagnostic comparison");
    let _ = writeln!(out, "stage         baseline      candidate     status");
    for r in &report.coverage {
        let _ = writeln!(
            out,
            "{:<13} {:>6}/{:<6} {:>6}/{:<6} {}",
            r.stage.label(), r.before.emitted, r.before.total,
            r.after.emitted, r.after.total, r.status.label()
        );
        let _ = writeln!(
            out,
            "  validated {} -> {}; pipelines {} -> {}",
            r.before.validated, r.after.validated, r.before.pipelines, r.after.pipelines
        );
    }
    let _ = writeln!(out, "Counts are not proof of shader identity or rendering correctness.");
    let _ = writeln!(out, "Validation/pipeline counts are informational. Match translate flags.");
    if let Some(gpu) = &report.gpu {
        let _ = writeln!(out, "\nGPU copy windows: {} -> {}", gpu.before_windows, gpu.after_windows);
        let _ = writeln!(out, "metric                         median old -> new      p95 old -> new       max old -> new      change");
        for r in &gpu.rows {
            let delta = r.delta.map_or_else(|| "n/a".to_owned(), |d| format!("{d:+.2}%"));
            let _ = writeln!(
                out, "{:<28} {:>8.2} -> {:<8.2} {:>8.2} -> {:<8.2} {:>8.2} -> {:<8.2} {}",
                r.metric.label(), r.before.median, r.after.median,
                r.before.p95, r.after.p95, r.before.max, r.after.max, delta
            );
        }
        let _ = writeln!(out, "Percentiles describe reporting windows, not individual frame times.");
        let _ = writeln!(out, "Match workload, phase and hardware. Less copying alone does not prove speedup.");
        if let (Some(limit), Some(status)) = (gpu.threshold, gpu.status) {
            let _ = writeln!(out, "Title copy median regression limit {limit:.2}%: {}", status.label());
        }
    }
    let _ = writeln!(out, "\nResult: {}", if report.passed() { "PASS" } else { "FAIL" });
    out
}
