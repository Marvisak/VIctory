//! Every title pipeline of the run and what became of its draws and
//! dispatches.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::json_str;

/// How a pipeline stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// Its host pipeline is made. For a draw that did not run: it could not
    /// be set up (bindings, state, vertex data).
    Ok,
    /// No draw has asked for it.
    NotRequested,
    Compiling,
    /// A shader of it does not translate.
    NotTranslated,
    /// Every shader translated; the host refused the pipeline.
    HostRefused,
    /// A shader of it is in `VICTORY_SKIP_PIPELINES`.
    Skipped,
}

impl Status {
    const ALL: [Status; 6] = [
        Status::Ok,
        Status::NotRequested,
        Status::Compiling,
        Status::NotTranslated,
        Status::HostRefused,
        Status::Skipped,
    ];

    fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NotRequested => "not_requested",
            Status::Compiling => "compiling",
            Status::NotTranslated => "not_translated",
            Status::HostRefused => "host_refused",
            Status::Skipped => "skipped",
        }
    }
}

/// The pipeline's status now (`None`: the title let go of it).
pub(crate) type StatusNow = Box<dyn Fn() -> Option<Status> + Send>;

struct Entry {
    /// Stages and shader hashes, as the log names pipelines.
    name: String,
    ran: u64,
    /// Draws and dispatches that did not run, by the status they met.
    missed: [u64; 6],
    status: StatusNow,
}

const SHARDS: usize = 16;
static MAP: [Mutex<Option<HashMap<usize, Entry>>>; SHARDS] = [const { Mutex::new(None) }; SHARDS];
/// Rows of released pipelines whose address was used again.
static RETIRED: Mutex<Vec<Row>> = Mutex::new(Vec::new());
/// Calls with no pipeline of ours set.
static NO_PIPELINE: AtomicU64 = AtomicU64::new(0);

fn shard(pipeline: usize) -> std::sync::MutexGuard<'static, Option<HashMap<usize, Entry>>> {
    MAP[(pipeline >> 6 ^ pipeline >> 12) % SHARDS]
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The title made pipeline `pipeline`.
pub(crate) fn created(pipeline: usize, name: String, status: StatusNow) {
    let entry = Entry {
        name,
        ran: 0,
        missed: [0; 6],
        status,
    };
    let old = shard(pipeline)
        .get_or_insert_with(HashMap::new)
        .insert(pipeline, entry);
    if let Some(old) = old {
        let row = row_of(&old);
        RETIRED.lock().unwrap_or_else(|e| e.into_inner()).push(row);
    }
}

/// A draw or dispatch of `pipeline`: it ran (`missed` `None`), or did not,
/// with the pipeline standing so.
pub(super) fn note(pipeline: usize, missed: Option<Option<Status>>) {
    let mut map = shard(pipeline);
    let Some(entry) = map.as_mut().and_then(|m| m.get_mut(&pipeline)) else {
        NO_PIPELINE.fetch_add(1, Relaxed);
        return;
    };
    match missed {
        None => entry.ran += 1,
        Some(met) => entry.missed[met.unwrap_or(Status::NotRequested) as usize] += 1,
    }
}

/// The name of `pipeline`, if it was noted.
pub(super) fn name_of(pipeline: usize) -> Option<String> {
    shard(pipeline)
        .as_ref()?
        .get(&pipeline)
        .map(|e| e.name.clone())
}

/// One pipeline in the report.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    name: String,
    status: Option<Status>,
    ran: u64,
    missed: [u64; 6],
}

impl Row {
    fn lost(&self) -> u64 {
        self.missed.iter().sum()
    }
}

fn row_of(entry: &Entry) -> Row {
    Row {
        name: entry.name.clone(),
        status: (entry.status)(),
        ran: entry.ran,
        missed: entry.missed,
    }
}

pub(super) struct Report {
    rows: Vec<Row>,
    no_pipeline: u64,
}

pub(super) fn report() -> Report {
    let mut rows = RETIRED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for s in &MAP {
        let map = s.lock().unwrap_or_else(|e| e.into_inner());
        rows.extend(map.iter().flatten().map(|(_, e)| row_of(e)));
    }
    rows.sort_by(|a, b| b.lost().cmp(&a.lost()).then_with(|| a.name.cmp(&b.name)));
    Report {
        rows,
        no_pipeline: NO_PIPELINE.load(Relaxed),
    }
}

/// The report's figures.
#[derive(Debug, Default, PartialEq, Eq)]
struct Totals {
    created: usize,
    /// Pipelines a draw or dispatch used, and those of them none of whose
    /// calls was lost.
    used: usize,
    used_whole: usize,
    ran: u64,
    missed: [u64; 6],
}

impl Report {
    fn totals(&self) -> Totals {
        let mut t = Totals {
            created: self.rows.len(),
            ..Totals::default()
        };
        for row in &self.rows {
            if row.ran + row.lost() > 0 {
                t.used += 1;
                t.used_whole += usize::from(row.lost() == 0);
            }
            t.ran += row.ran;
            for (sum, n) in t.missed.iter_mut().zip(row.missed) {
                *sum += n;
            }
        }
        t
    }

    fn by_status(missed: &[u64; 6]) -> String {
        Status::ALL
            .iter()
            .filter(|&&s| missed[s as usize] > 0)
            .map(|&s| {
                let name = if s == Status::Ok {
                    "not_set_up"
                } else {
                    s.name()
                };
                format!("{name} {}", missed[s as usize])
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn text(&self) -> String {
        let t = self.totals();
        let lost: u64 = t.missed.iter().sum();
        let mut out = String::from("VICTORY_PROBE pipeline coverage\n");
        let _ = writeln!(
            out,
            "pipelines made by the title: {}, used: {}, used with no call lost: {}",
            t.created, t.used, t.used_whole
        );
        let _ = writeln!(
            out,
            "draws and dispatches: {} ran, {lost} did not ({}); without a pipeline: {}",
            t.ran,
            Self::by_status(&t.missed),
            self.no_pipeline
        );
        let _ = writeln!(out, "\npipelines that lost the most calls:");
        for row in self.rows.iter().filter(|r| r.lost() > 0).take(20) {
            let _ = writeln!(
                out,
                "lost {} (ran {}; {}) now {}: {}",
                row.lost(),
                row.ran,
                Self::by_status(&row.missed),
                row.status.map_or("released", Status::name),
                row.name
            );
        }
        out
    }

    /// Every pipeline used, most calls lost first.
    pub(super) fn json(&self) -> String {
        let t = self.totals();
        let missed = |m: &[u64; 6]| {
            Status::ALL
                .iter()
                .map(|&s| format!("\"{}\":{}", s.name(), m[s as usize]))
                .collect::<Vec<_>>()
                .join(",")
        };
        let rows: Vec<String> = self
            .rows
            .iter()
            .filter(|r| r.ran + r.lost() > 0)
            .map(|r| {
                format!(
                    "{{\"name\":{},\"status\":{},\"ran\":{},\"missed\":{{{}}}}}",
                    json_str(&r.name),
                    json_str(r.status.map_or("released", Status::name)),
                    r.ran,
                    missed(&r.missed)
                )
            })
            .collect();
        format!(
            "{{\"created\":{},\"used\":{},\"used_whole\":{},\"ran\":{},\"missed\":{{{}}},\"no_pipeline\":{},\n\"pipelines\":[\n{}\n]}}\n",
            t.created,
            t.used,
            t.used_whole,
            t.ran,
            missed(&t.missed),
            self.no_pipeline,
            rows.join(",\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, ran: u64, missed: [u64; 6]) -> Row {
        Row {
            name: name.into(),
            status: Some(Status::Ok),
            ran,
            missed,
        }
    }

    #[test]
    fn calls_are_counted_by_what_they_met() {
        let report = Report {
            rows: vec![
                row("b", 0, [0, 0, 30, 0, 0, 0]),
                row("c", 5, [2, 0, 0, 0, 0, 0]),
                row("a", 900, [0; 6]),
                row("d", 0, [0; 6]),
            ],
            no_pipeline: 1,
        };
        let t = report.totals();
        assert_eq!((t.created, t.used, t.used_whole, t.ran), (4, 3, 1, 905));
        assert_eq!(t.missed, [2, 0, 30, 0, 0, 0]);
        let text = report.text();
        assert!(text.contains("905 ran, 32 did not (not_set_up 2, compiling 30)"));
        assert!(text.contains("without a pipeline: 1"));
        assert!(text.contains("lost 30 (ran 0; compiling 30) now ok: b"));
        let json = report.json();
        // The unused pipeline is not listed.
        assert_eq!(json.matches("\"name\":").count(), 3);
        assert!(json.contains("\"compiling\":30"));
    }

    #[test]
    fn a_pipeline_made_again_at_the_same_address_keeps_both_rows() {
        // An address no pipeline has.
        let at = 0x40;
        created(at, "first".into(), Box::new(|| None));
        note(at, None);
        note(at, Some(Some(Status::HostRefused)));
        assert_eq!(name_of(at).as_deref(), Some("first"));
        created(at, "second".into(), Box::new(|| Some(Status::Compiling)));
        note(at, Some(Some(Status::Compiling)));
        let report = report();
        let find = |name: &str| report.rows.iter().find(|r| r.name == name).unwrap();
        assert_eq!(find("first").ran, 1);
        assert_eq!(find("first").missed[Status::HostRefused as usize], 1);
        assert_eq!(find("second").status, Some(Status::Compiling));
        assert_eq!(find("second").lost(), 1);
    }
}
