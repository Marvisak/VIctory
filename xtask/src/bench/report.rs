use std::collections::BTreeMap;
use std::fmt::Write;
use std::time::Duration;

use super::Args;
use anyhow::{Result, ensure};

const MIB: f64 = 1024. * 1024.;
const MARKER: &str = "<!-- victory-bench-v1 -->";
const METRICS: [&str; 6] = [
    "translation failures",
    "texture creation failures",
    "peak draws/frame",
    "peak fps",
    "peak RSS (MiB)",
    "crash RVA",
];

#[derive(Default)]
pub(super) struct Report {
    started: bool,
    translation: u64,
    textures: u64,
    reasons: BTreeMap<String, u64>,
    draws: Option<u64>,
    fps: Option<f64>,
    rss: Option<u64>,
    crash: Option<String>,
    timeline: String,
}

impl Report {
    pub fn started() -> Self {
        Self {
            started: true,
            ..Self::default()
        }
    }

    pub fn event(&mut self, line: &str) {
        // tracing may color the level, target and fields, even when redirected.
        if let Some((before, rest)) = line.split_once("\x1b[") {
            let mut plain = String::from(before);
            for span in rest.split("\x1b[") {
                let Some((codes, text)) = span.split_once('m') else {
                    return;
                };
                if !codes.bytes().all(|b| b.is_ascii_digit() || b == b';') {
                    return;
                }
                plain.push_str(text);
            }
            self.plain_event(&plain);
        } else {
            self.plain_event(line);
        }
    }

    fn plain_event(&mut self, line: &str) {
        if let Some(message) = line.strip_prefix("VIctory: unhandled exception ") {
            let rva = message
                .split_once(" at ")
                .and_then(|(_, site)| site.split_once('+'))
                .and_then(|(_, value)| value.split_whitespace().next())
                .filter(|s| valid_rva(s));
            self.crash = Some(rva.unwrap_or("unknown").into());
        } else if let Some((_, fields)) = line.split_once(TRANSLATION) {
            self.translation += 1;
            let reason = translation_reason(fields).unwrap_or("other translation failure");
            *self.reasons.entry(reason.into()).or_default() += 1;
        } else if line.contains(" WARN victory_gpu::resource: no host texture ") {
            self.textures += 1;
        } else if let Some((_, message)) = line.split_once(" INFO victory_gpu::host: ")
            && let Some((fps, rest)) = message.split_once(" fps, ")
            && let Ok(fps) = fps.parse::<f64>()
            && fps.is_finite()
            && fps >= 0.
            && let Some((before, _)) = rest.split_once(" draws/frame,")
            && let Some(draws) = before
                .split_whitespace()
                .next_back()
                .and_then(|s| s.parse::<u64>().ok())
        {
            self.fps = Some(self.fps.unwrap_or_default().max(fps));
            self.draws = Some(self.draws.unwrap_or_default().max(draws));
        }
    }

    pub fn sample(&mut self, elapsed: Duration, rss: Option<u64>, free: u64) {
        if let Some(bytes) = rss {
            self.rss = Some(self.rss.unwrap_or_default().max(bytes));
        }
        let rss = rss.map_or_else(|| "-".to_owned(), |v| format!("{:.1}", v as f64 / MIB));
        let counts = if self.started {
            format!("{} | {}", self.translation, self.textures)
        } else {
            "- | -".to_owned()
        };
        writeln!(
            self.timeline,
            "| {:.1} | {rss} | {:.1} | {counts} |",
            elapsed.as_secs_f64(),
            free as f64 / MIB,
        )
        .unwrap();
    }

    pub fn markdown(&self, args: &Args, stop: &str, previous: Option<&[String]>) -> String {
        let values = [
            self.started.then(|| self.translation.to_string()),
            self.started.then(|| self.textures.to_string()),
            self.draws.map(|v| v.to_string()),
            self.fps.map(|v| format!("{v:.1}")),
            self.rss.map(|v| format!("{:.2}", v as f64 / MIB)),
            self.started
                .then(|| self.crash.as_deref().unwrap_or("none").into()),
        ]
        .map(|v| v.unwrap_or_else(|| "unobserved".into()));
        let mut out = format!(
            "# VIctory benchmark\n{MARKER}\n\nBuild label: {}\n\nConsole: {}. Time budget: {} s. RSS limit: {} GiB. Free-memory floor: {} GiB.\n\nStop: {stop}.\n\n| Metric | Value |\n| --- | ---: |\n",
            args.label, args.console, args.seconds, args.max_ram_gb, args.min_sys_gb
        );
        for (metric, v) in METRICS.iter().zip(&values) {
            writeln!(out, "| {metric} | {v} |").unwrap();
        }
        out.push_str("\n## Translation failures by reason\n\n");
        if self.reasons.is_empty() {
            out.push_str("None recorded.\n");
        }
        for (reason, count) in &self.reasons {
            writeln!(out, "- {count} x {reason}").unwrap();
        }
        out.push_str("\n## Timeline\n\n| Elapsed (s) | RSS (MiB) | Available (MiB) | Translation failures | Texture failures |\n| ---: | ---: | ---: | ---: | ---: |\n");
        out.push_str(&self.timeline);
        if let Some(prev) = previous {
            out.push_str("\n## Baseline comparison\n\n| Metric | Baseline | This run |\n| --- | ---: | ---: |\n");
            for ((metric, v), old) in METRICS.iter().zip(&values).zip(prev) {
                writeln!(out, "| {metric} | {old} | {v} |").unwrap();
            }
        }
        out
    }
}

const TRANSLATION: &str =
    " WARN victory_gpu::pipeline: shader does not translate; its pipelines' draws are dropped ";

fn translation_reason(fields: &str) -> Option<&str> {
    let (_, fields) = fields.split_once(" error=")?;
    let (error, _) = fields
        .split_once(" cached=")
        .or_else(|| fields.split_once(" ms="))?;
    let reason = if let Some(location) = error.strip_prefix("GFX10 code at ") {
        location.split_once(": ")?.1
    } else {
        error.strip_prefix("malformed shader: ").unwrap_or(error)
    };
    // Keep only translator messages. Offsets, hashes and diagnostic context stay out of reports.
    (!reason.is_empty()
        && reason.len() <= 180
        && reason
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" ()_,.'-".contains(&b)))
    .then_some(reason)
}

fn valid_rva(s: &str) -> bool {
    s.strip_prefix("0x")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn baseline(text: &str) -> Result<Vec<String>> {
    ensure!(
        text.lines().any(|s| s == MARKER),
        "unrecognized benchmark baseline"
    );
    METRICS
        .iter()
        .enumerate()
        .map(|(i, metric)| {
            let prefix = format!("| {metric} | ");
            let mut rows = text
                .lines()
                .filter_map(|line| line.strip_prefix(&prefix)?.strip_suffix(" |"))
                .filter(|s| !s.contains('|')); // Ignore the three-column comparison table.
            let s = rows.next().unwrap_or("");
            let valid = s == "unobserved"
                || match i {
                    0..=2 => s.parse::<u64>().is_ok(),
                    3 | 4 => s.parse::<f64>().is_ok_and(|x| x.is_finite() && x >= 0.),
                    _ => matches!(s, "unknown" | "none") || valid_rva(s),
                };
            ensure!(
                valid && rows.next().is_none(),
                "invalid benchmark baseline for {metric}"
            );
            Ok(s.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn aggregate_and_compare_reports_without_raw_log_lines() {
        let crate::Cmd::Bench(args) =
            crate::Cli::try_parse_from(["xtask", "bench", "--title", "example"])
                .unwrap()
                .command
        else {
            unreachable!()
        };
        let mut report = Report::default();
        report.sample(Duration::ZERO, None, 2 << 30);
        let refused = report.markdown(&args, "minimum free system memory", None);
        assert_eq!(baseline(&refused).unwrap(), ["unobserved"; 6]);
        assert!(refused.contains("| 0.0 | - | 2048.0 | - | - |"));
        let mut report = Report::started();
        let old = baseline(&report.markdown(&args, "time limit", None)).unwrap();
        assert_eq!(
            old,
            ["0", "0", "unobserved", "unobserved", "unobserved", "none"]
        );
        report.event(" INFO victory_gpu::pipeline: unrelated message");
        for _ in 0..2 {
            report.event(" WARN victory_gpu::pipeline: shader does not translate; its pipelines' draws are dropped hash=123 error=GFX10 code at 0x10: an unknown stride cached=false");
        }
        report.event(" WARN victory_gpu::resource: no host texture error=private-name");
        report.event(" \x1b[32mINFO\x1b[0m victory_gpu::host: 7.5 fps, 133.3 ms/frame (other fields), 18 draws/frame, dropped 0");
        report.event(" INFO victory_gpu::host: 30.1 fps, 33.2 ms/frame (other fields), 2 draws/frame, dropped 0");
        report.event("VIctory: unhandled exception 0x80000003 at private-name.dll+0x4f");
        report.sample(Duration::ZERO, Some(3 << 20), 4 << 30);
        let markdown = report.markdown(&args, "process exit 7", Some(&old));
        assert!(!markdown.contains("private-name") && !markdown.contains("hash="));
        assert!(markdown.contains("- 2 x an unknown stride"));
        assert!(markdown.contains("| peak fps | unobserved | 30.1 |"));
        assert_eq!(
            baseline(&markdown).unwrap(),
            ["2", "1", "18", "30.1", "3.00", "0x4f"]
        );
        assert!(baseline(&markdown.replace("| peak fps | 30.1 |", "| peak fps | bad |")).is_err());
    }
}
