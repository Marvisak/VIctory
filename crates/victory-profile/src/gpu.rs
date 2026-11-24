//! GPU work as the host reports it: the noted commands timed on the GPU
//! (`VICTORY_GPU_TIMING` in a diagnostics build), the slowest submission,
//! and draws and dispatches dropped or of zero groups.
//!
//! Commands are known by their marker text cut to a key ([`command_key`]):
//! the kind, its groups and the pipeline's shader hashes, with addresses
//! and other hexadecimal values blanked, so a report pastes without
//! title data.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use crate::stats::{Ring, Summary};
use crate::{DROPPED, WORK, ZERO_GROUPS, lock, on};

/// Longest key kept, in characters.
const KEY_CHARS: usize = 160;
/// Distinct commands kept per table; later ones are counted as others.
const MAX_COMMANDS: usize = 4096;
/// Times kept per command for its median.
const SAMPLES: usize = 64;
/// Commands in a summary.
pub(crate) const WORST: usize = 8;

/// A marker's text cut to what names the command: before the first
/// ` | ` (the per-stage user data that follows holds title values), each
/// hexadecimal number shown as `0x_`, at most [`KEY_CHARS`] characters.
pub fn command_key(text: &str) -> String {
    let head = text.split(" | ").next().unwrap_or_default().trim();
    let mut out = String::with_capacity(head.len().min(KEY_CHARS));
    let mut chars = head.chars().peekable();
    let mut previous = ' ';
    while let Some(c) = chars.next() {
        if c == '0' && chars.peek() == Some(&'x') && !previous.is_ascii_alphanumeric() {
            chars.next();
            while chars.peek().is_some_and(char::is_ascii_hexdigit) {
                chars.next();
            }
            out.push_str("0x_");
            previous = '_';
            continue;
        }
        out.push(c);
        previous = c;
    }
    if out.chars().count() > KEY_CHARS {
        out = out.chars().take(KEY_CHARS - 1).collect();
        out.push('~');
    }
    out
}

/// One command's GPU times.
#[derive(Debug, Clone)]
pub(crate) struct Command {
    pub(crate) runs: u64,
    pub(crate) total_ms: f64,
    pub(crate) max_ms: f64,
    /// Its list's GPU time when it took longest.
    pub(crate) list_ms: f64,
    samples: Ring<f64>,
}

impl Command {
    fn new() -> Self {
        Command {
            runs: 0,
            total_ms: 0.0,
            max_ms: 0.0,
            list_ms: 0.0,
            samples: Ring::new(SAMPLES),
        }
    }

    fn add(&mut self, ms: f64, list_ms: f64) {
        self.runs += 1;
        self.total_ms += ms;
        if ms >= self.max_ms {
            self.max_ms = ms;
            self.list_ms = list_ms;
        }
        self.samples.push(ms);
    }

    /// The median of its last times.
    pub(crate) fn median_ms(&self) -> f64 {
        let samples: Vec<f64> = self.samples.iter().copied().collect();
        Summary::of(&samples).map_or(0.0, |s| s.p50)
    }
}

/// Commands by key, and how many runs found the table full.
#[derive(Default)]
pub(crate) struct Commands {
    by_key: HashMap<String, Command>,
    others: u64,
}

impl Commands {
    pub(crate) fn add(&mut self, key: &str, ms: f64, list_ms: f64) {
        if let Some(c) = self.by_key.get_mut(key) {
            c.add(ms, list_ms);
        } else if self.by_key.len() < MAX_COMMANDS {
            let mut c = Command::new();
            c.add(ms, list_ms);
            self.by_key.insert(key.to_owned(), c);
        } else {
            self.others += 1;
        }
    }

    /// The `n` that took longest at their worst, worst first (ties by
    /// total time, then key).
    pub(crate) fn worst(&self, n: usize) -> Vec<(&str, &Command)> {
        let mut all: Vec<(&str, &Command)> =
            self.by_key.iter().map(|(k, c)| (k.as_str(), c)).collect();
        all.sort_by(|a, b| {
            b.1.max_ms
                .total_cmp(&a.1.max_ms)
                .then(b.1.total_ms.total_cmp(&a.1.total_ms))
                .then(a.0.cmp(b.0))
        });
        all.truncate(n);
        all
    }

    /// Their lines: one per command, for a summary or an issue.
    pub(crate) fn lines(&self, n: usize, title: &str) -> Vec<String> {
        if self.by_key.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "{title}: worst {} of {} commands by GPU time (max, median, total ms; runs; \
             list ms at the max)",
            n.min(self.by_key.len()),
            self.by_key.len()
        )];
        for (i, (key, c)) in self.worst(n).into_iter().enumerate() {
            lines.push(format!(
                "  {}. {:.2} / {:.2} / {:.1} ms; {} runs; list {:.1} ms: {key}",
                i + 1,
                c.max_ms,
                c.median_ms(),
                c.total_ms,
                c.runs,
                c.list_ms
            ));
        }
        if self.others > 0 {
            lines.push(format!(
                "  ({} runs of further commands not kept)",
                self.others
            ));
        }
        lines
    }
}

/// Since the last summary, and since the start.
static PERIOD: Mutex<Option<Commands>> = Mutex::new(None);
static ALL: Mutex<Option<Commands>> = Mutex::new(None);

/// A noted command that took `ms` on the GPU, in a list that took
/// `list_ms` (`VICTORY_GPU_TIMING`: lists run one at a time). `text` is
/// its marker text; only its key is kept.
pub fn command(text: &str, ms: f64, list_ms: f64) {
    if !on() {
        return;
    }
    let key = command_key(text);
    for table in [&PERIOD, &ALL] {
        lock(table)
            .get_or_insert_with(Commands::default)
            .add(&key, ms, list_ms);
    }
    crate::live::command(&key, ms, list_ms);
}

/// A draw, mesh dispatch or dispatch the host did not run.
#[inline]
pub fn dropped() {
    if on() {
        WORK[DROPPED].fetch_add(1, Relaxed);
    }
}

/// A draw, mesh dispatch or dispatch of zero groups or vertices (it does
/// nothing: usually culling that let nothing through).
#[inline]
pub fn zero_groups() {
    if on() {
        WORK[ZERO_GROUPS].fetch_add(1, Relaxed);
    }
}

/// The slowest submission of a period: GPU wait, lists, its pipelines.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Slowest {
    pub(crate) ms: f64,
    pub(crate) lists: usize,
    pub(crate) pipelines: String,
}

static SLOWEST: Mutex<Option<Slowest>> = Mutex::new(None);

/// A finished submission: `gpu` its fence wait, `pipelines` (asked only
/// for the slowest so far of a period) the pipelines its lists bound.
pub fn submission_done(gpu: Duration, lists: usize, pipelines: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let ms = gpu.as_secs_f64() * 1e3;
    let summary_wants = lock(&SLOWEST).as_ref().is_none_or(|s| ms > s.ms);
    let live_wants = crate::live::wants_submission(ms);
    if !summary_wants && !live_wants {
        return;
    }
    let s = Slowest {
        ms,
        lists,
        pipelines: pipelines(),
    };
    if summary_wants {
        let mut slowest = lock(&SLOWEST);
        if slowest.as_ref().is_none_or(|old| ms > old.ms) {
            *slowest = Some(s.clone());
        }
    }
    if live_wants {
        crate::live::submission(s);
    }
}

/// The summary's lines, over the period (taken) or since the start.
pub(crate) fn lines(all: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let slowest = if all { None } else { lock(&SLOWEST).take() };
    if let Some(s) = slowest {
        lines.push(format!(
            "slowest submission: {:.1} ms, {} lists; pipelines {}",
            s.ms, s.lists, s.pipelines
        ));
    }
    let commands = if all {
        lock(&ALL)
            .as_ref()
            .map(|c| c.lines(2 * WORST, "GPU commands since the start"))
    } else {
        lock(&PERIOD)
            .take()
            .map(|c| c.lines(WORST, "GPU commands of the period"))
    };
    lines.extend(commands.unwrap_or_default());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_the_command_and_blank_values() {
        let text = "dispatch 64x1x1 [compute 8bbcbda6008eaa37] | 8bbcbda6008eaa37: u0=0x1234 \
                    [heap 3: srv] root2=0x7ff0000 host Some(0xabc)";
        assert_eq!(
            command_key(text),
            "dispatch 64x1x1 [compute 8bbcbda6008eaa37]"
        );
        assert_eq!(
            command_key("copy 4096 bytes from 0xdeadbeef to 0x10"),
            "copy 4096 bytes from 0x_ to 0x_"
        );
        // A hash that has `0x` inside it stays whole.
        assert_eq!(command_key("mesh 3 [mesh a0x1f]"), "mesh 3 [mesh a0x1f]");
        let long = "x".repeat(400);
        let key = command_key(&long);
        assert_eq!(key.chars().count(), KEY_CHARS);
        assert!(key.ends_with('~'));
    }

    #[test]
    fn the_worst_commands_come_first_with_their_median() {
        let mut c = Commands::default();
        for ms in [1.0, 2.0, 3.0] {
            c.add("dispatch 1x1x1 [compute a]", ms, 10.0);
        }
        c.add("mesh 3 [mesh b, pixel c]", 387.8, 400.0);
        c.add("mesh 3 [mesh b, pixel c]", 12.0, 30.0);
        let worst = c.worst(5);
        assert_eq!(worst[0].0, "mesh 3 [mesh b, pixel c]");
        assert_eq!(worst[0].1.runs, 2);
        assert_eq!(worst[0].1.list_ms, 400.0);
        assert_eq!(worst[1].1.median_ms(), 2.0);
        let lines = c.lines(1, "test");
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("test: worst 1 of 2 commands"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "  1. 387.80 / 12.00 / 399.8 ms; 2 runs; list 400.0 ms: mesh 3 [mesh b, pixel c]"
        );
    }

    #[test]
    fn a_full_table_counts_the_rest() {
        let mut c = Commands::default();
        for i in 0..MAX_COMMANDS + 3 {
            c.add(&format!("k{i}"), 1.0, 1.0);
        }
        assert_eq!(c.by_key.len(), MAX_COMMANDS);
        assert_eq!(c.others, 3);
        assert!(
            c.lines(1, "t")
                .last()
                .unwrap()
                .contains("3 runs of further commands")
        );
    }
}
