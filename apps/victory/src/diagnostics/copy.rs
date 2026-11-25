use super::numbers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Metric {
    Submissions,
    TitleCopyMb,
    RefreshMb,
    SnapshotMb,
    WorkingSetMb,
    HardFaults,
}

impl Metric {
    pub(super) const ALL: [Self; 6] = [
        Self::Submissions,
        Self::TitleCopyMb,
        Self::RefreshMb,
        Self::SnapshotMb,
        Self::WorkingSetMb,
        Self::HardFaults,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Submissions => "submissions/frame",
            Self::TitleCopyMb => "title copy MB/frame",
            Self::RefreshMb => "refresh MB/frame",
            Self::SnapshotMb => "snapshot MB/frame",
            Self::WorkingSetMb => "working set MB",
            Self::HardFaults => "hard faults/frame",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Submissions => 0,
            Self::TitleCopyMb => 1,
            Self::RefreshMb => 2,
            Self::SnapshotMb => 3,
            Self::WorkingSetMb => 4,
            Self::HardFaults => 5,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Samples {
    rows: Vec<[f64; 6]>,
}

impl Samples {
    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn values(&self, metric: Metric) -> Vec<f64> {
        self.rows.iter().map(|row| row[metric.index()]).collect()
    }
}

/// One observation is one existing copystat reporting window, not a frame.
/// Prefixes from tracing (timestamps, log level, target) are ignored.
pub(super) fn parse(text: &str) -> Result<Samples, String> {
    let mut out = Samples::default();
    for (i, line) in text.lines().enumerate() {
        let Some((_, row)) = line.split_once("copies per frame:") else {
            continue;
        };
        let values = parse_row(row).ok_or_else(|| format!("invalid copy statistics at line {}", i + 1))?;
        out.rows.push(values);
    }
    if out.rows.is_empty() {
        return Err("no complete copy statistics windows".into());
    }
    Ok(out)
}

fn parse_row(row: &str) -> Option<[f64; 6]> {
    Some([
        numbers::before(row, " submissions")?,
        numbers::after(row, "title copies ", "MB")?,
        numbers::after(row, "refreshes ", "MB")?,
        numbers::after(row, "snapshots ", "MB")?,
        numbers::after(row, "working set ", "MB")?,
        numbers::before(row, " hard faults")?,
    ])
}
