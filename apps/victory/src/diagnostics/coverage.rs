use super::numbers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Graphics,
    Compute,
    Mesh,
}

impl Stage {
    pub(super) const ALL: [Self; 3] = [Self::Graphics, Self::Compute, Self::Mesh];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Graphics => "vertex/pixel",
            Self::Compute => "compute",
            Self::Mesh => "mesh",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Graphics => 0,
            Self::Compute => 1,
            Self::Mesh => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Counts {
    pub emitted: u64,
    pub total: u64,
    pub validated: u64,
    pub pipelines: u64,
}

#[derive(Debug)]
pub(super) struct Coverage([Counts; 3]);

impl Coverage {
    pub(super) fn get(&self, stage: Stage) -> Counts {
        self.0[stage.index()]
    }
}

/// The three summary rows emitted by the existing offline translate command.
/// Reject missing, repeated or inconsistent rows; a partial log is not a pass.
pub(super) fn parse(text: &str) -> Result<Coverage, String> {
    let mut rows = [None; 3];
    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        let row = line.strip_prefix("DXIL").map(str::trim_start).unwrap_or(line);
        if !row.contains(" emitted of ") {
            continue;
        }
        let (stage, counts) = parse_row(row)
            .ok_or_else(|| format!("invalid DXIL summary at line {}", line_no + 1))?;
        if rows[stage.index()].replace(counts).is_some() {
            return Err(format!("duplicate {} summary", stage.label()));
        }
    }
    let [Some(graphics), Some(compute), Some(mesh)] = rows else {
        return Err("incomplete census: expected graphics, compute and mesh summaries".into());
    };
    if [graphics, compute, mesh].iter().all(|c| c.total == 0) {
        return Err("census contains no eligible programs".into());
    }
    Ok(Coverage([graphics, compute, mesh]))
}

fn parse_row(row: &str) -> Option<(Stage, Counts)> {
    let (emitted, rest) = row.split_once(" emitted of ")?;
    let (total_stage, checks) = rest.split_once(" programs; ")?;
    let (total, stage) = total_stage.split_once(' ')?;
    let stage = match stage {
        "vertex and pixel" => Stage::Graphics,
        "compute" => Stage::Compute,
        "mesh" => Stage::Mesh,
        _ => return None,
    };
    let (validated, pipelines) = checks.split_once(" validated, ")?;
    let counts = Counts {
        emitted: numbers::count(emitted)?,
        total: numbers::count(total)?,
        validated: numbers::count(validated)?,
        pipelines: numbers::count(pipelines.strip_suffix(" pipelines")?)?,
    };
    (counts.emitted <= counts.total
        && counts.validated <= counts.emitted
        && counts.pipelines <= counts.emitted)
        .then_some((stage, counts))
}
