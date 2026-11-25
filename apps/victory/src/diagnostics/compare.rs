use super::copy::{Metric, Samples};
use super::coverage::{Counts, Coverage, Stage};
use super::stats::{self, Stats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Pass,
    Regressed,
    Incomparable,
}

impl Status {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Regressed => "REGRESSION",
            Self::Incomparable => "INCOMPARABLE",
        }
    }
}

#[derive(Debug)]
pub(super) struct CoverageRow {
    pub stage: Stage,
    pub before: Counts,
    pub after: Counts,
    pub status: Status,
}

#[derive(Debug)]
pub(super) struct MetricRow {
    pub metric: Metric,
    pub before: Stats,
    pub after: Stats,
    pub delta: Option<f64>,
}

#[derive(Debug)]
pub(super) struct GpuReport {
    pub before_windows: usize,
    pub after_windows: usize,
    pub rows: Vec<MetricRow>,
    pub threshold: Option<f64>,
    pub status: Option<Status>,
}

#[derive(Debug)]
pub(super) struct Report {
    pub coverage: Vec<CoverageRow>,
    pub gpu: Option<GpuReport>,
}

impl Report {
    pub(super) fn passed(&self) -> bool {
        self.coverage.iter().all(|r| r.status == Status::Pass)
            && self.gpu.as_ref().and_then(|g| g.status).is_none_or(|s| s == Status::Pass)
    }
}

pub(super) fn build(
    before: &Coverage,
    after: &Coverage,
    gpu: Option<&(Samples, Samples)>,
    threshold: Option<f64>,
) -> Report {
    let coverage = Stage::ALL
        .into_iter()
        .map(|stage| {
            let before = before.get(stage);
            let after = after.get(stage);
            let status = if before.total != after.total {
                Status::Incomparable
            } else if after.emitted < before.emitted {
                Status::Regressed
            } else {
                Status::Pass
            };
            CoverageRow { stage, before, after, status }
        })
        .collect();
    let gpu = gpu.map(|(a, b)| {
        let rows: Vec<_> = Metric::ALL
            .into_iter()
            .map(|metric| {
                let before = stats::summarize(a.values(metric)).expect("validated nonempty samples");
                let after = stats::summarize(b.values(metric)).expect("validated nonempty samples");
                let delta = stats::percent(before.median, after.median);
                MetricRow { metric, before, after, delta }
            })
            .collect();
        let status = threshold.map(|limit| {
            let row = rows.iter().find(|r| r.metric == Metric::TitleCopyMb).expect("copy metric");
            if row.delta.is_some_and(|d| d <= limit) {
                Status::Pass
            } else {
                Status::Regressed
            }
        });
        GpuReport {
            before_windows: a.len(),
            after_windows: b.len(),
            rows,
            threshold,
            status,
        }
    });
    Report { coverage, gpu }
}
