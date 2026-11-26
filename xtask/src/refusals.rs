//! `cargo xtask refusals <log>`: what a run's log says was refused, dropped,
//! or never translated, grouped and counted.
//!
//! Four line shapes are read:
//!
//! - `host pipeline refused`, `host mesh pipeline refused` and
//!   `host root signature refused`, with the shaders they were for;
//! - `D3D12: ... id=... severity=...`, the debug layer's own words - a run
//!   only has them with `VICTORY_D3D12_DEBUG=1`;
//! - `shader does not translate; its pipelines' draws are dropped`;
//! - `dropped ... without a host pipeline: ...`, one per reason.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

/// One refused pipeline: what it was for, and what the device said.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Refusal {
    /// `compute`, `mesh`, or the root signature's own line.
    pub kind: String,
    /// The shaders the refused pipeline was made of, as the log names them.
    pub shaders: String,
    /// The HRESULT's text as the log prints it.
    pub error: String,
}

/// One debug-layer message, with its count.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DebugMessage {
    pub text: String,
    pub id: u64,
    pub severity: u64,
}

/// A shader the translator cannot express.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Untranslated {
    pub stage: String,
    pub hash: String,
    pub error: String,
}

/// What a log says, grouped.
#[derive(Debug, Default)]
pub(crate) struct Report {
    pub refusals: BTreeMap<Refusal, u64>,
    pub debug: BTreeMap<DebugMessage, u64>,
    pub untranslated: BTreeMap<Untranslated, u64>,
    pub drops: BTreeMap<String, f64>,
}

/// Reads a whole log into a [`Report`].
pub(crate) fn parse(log: &str) -> Report {
    let mut report = Report::default();
    for line in log.lines() {
        if let Some(rest) = line.split_once("host pipeline refused error=") {
            push_refusal(&mut report, "pipeline", rest.1);
        } else if let Some(rest) = line.split_once("host mesh pipeline refused error=") {
            push_refusal(&mut report, "mesh", rest.1);
        } else if let Some(rest) = line.split_once("host root signature refused error=") {
            push_refusal(&mut report, "root signature", rest.1);
        } else if let Some(rest) = line.split_once("shader does not translate") {
            read_untranslated(&mut report, rest.1);
        } else if line.contains("without a host pipeline: ") {
            read_drop(&mut report, line);
        } else if let Some(at) = line.find("D3D12: ") {
            read_debug(&mut report, &line[at + "D3D12: ".len()..]);
        }
    }
    report
}

fn push_refusal(report: &mut Report, kind: &str, rest: &str) {
    // Every refusal line logs `?shaders`, so the list is there in practice; the
    // fallback below is only for a log that stopped before it.
    let (error, shaders) = match rest.split_once(" shaders=[") {
        Some((error, list)) => (error, list.split_once(']').map_or(list, |(list, _)| list)),
        None => (rest.split(" messages=").next().unwrap_or(rest), ""),
    };
    let refusal = Refusal {
        kind: kind.to_owned(),
        shaders: shaders.to_owned(),
        error: error.trim().to_owned(),
    };
    *report.refusals.entry(refusal).or_insert(0) += 1;
}

fn read_untranslated(report: &mut Report, rest: &str) {
    let field = |name: &str| -> String {
        let start = rest.find(name).map(|at| at + name.len());
        let Some(start) = start else {
            return String::new();
        };
        let tail = &rest[start..];
        match name {
            // A quoted value: stage="compute".
            "stage=\"" => tail.split('"').next().unwrap_or("").to_owned(),
            // An unquoted value that runs to the next space.
            _ => tail.split(' ').next().unwrap_or("").to_owned(),
        }
    };
    // The real line carries cached=, ms=, user_ms= and kernel_ms= after
    // the error, so the error stops at the first of them: the same shader with
    // different timings is one kind, not one per timing.
    let untranslated = Untranslated {
        stage: field("stage=\""),
        hash: field("hash="),
        error: match rest.split_once("error=") {
            Some((_, error)) => error
                .split(" cached=")
                .next()
                .unwrap_or(error)
                .trim()
                .to_owned(),
            None => String::new(),
        },
    };
    *report.untranslated.entry(untranslated).or_insert(0) += 1;
}

fn read_drop(report: &mut Report, line: &str) {
    let Some((before, why)) = line.split_once("without a host pipeline: ") else {
        return;
    };
    // "dropped 227.7/frame without a host pipeline: <why>"
    let rate = before
        .rsplit_once("dropped ")
        .and_then(|(_, tail)| tail.split_once("/frame"))
        .and_then(|(rate, _)| rate.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let entry = report.drops.entry(why.trim().to_owned()).or_insert(0.0);
    *entry = entry.max(rate);
}

fn read_debug(report: &mut Report, rest: &str) {
    let id = u64_field(rest, " id=").unwrap_or(u64::MAX);
    let severity = u64_field(rest, " severity=").unwrap_or(u64::MAX);
    let text = match rest.find(" id=") {
        Some(at) => rest[..at].trim(),
        None => rest.trim(),
    };
    if text.is_empty() {
        return;
    }
    let message = DebugMessage {
        text: text.to_owned(),
        id,
        severity,
    };
    *report.debug.entry(message).or_insert(0) += 1;
}

fn u64_field(line: &str, name: &str) -> Option<u64> {
    let tail = &line[line.find(name)? + name.len()..];
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Prints a [`Report`].
pub(crate) fn print(report: &Report) {
    print!("{}", render(report));
}

/// The report as text, so a test can read what a reader would.
pub(crate) fn render(report: &Report) -> String {
    let mut out = String::new();
    if report.refusals.is_empty() && report.untranslated.is_empty() && report.drops.is_empty() {
        out.push_str("nothing refused, dropped, or untranslated in this log\n");
    }
    if !report.refusals.is_empty() {
        out.push_str(&format!("refused pipelines ({} kinds)\n", report.refusals.len()));
        for (refusal, count) in sorted(&report.refusals) {
            // A pipeline's line names its shaders (`compute 2da4…`, `mesh …, pixel …`),
            // which is more use than the word "pipeline"; the root signature's
            // line names none.
            let what = match refusal.shaders.is_empty() {
                true => refusal.kind.clone(),
                false => refusal.shaders.clone(),
            };
            out.push_str(&format!("  {count:>5}x [{what}] {}\n", refusal.error));
        }
    }
    if !report.debug.is_empty() {
        out.push_str(&format!("\nthe debug layer's reasons ({} kinds)\n", report.debug.len()));
        for (message, count) in sorted(&report.debug) {
            out.push_str(&format!(
                "  {count:>5}x id={} severity={}: {}\n",
                message.id, message.severity, message.text
            ));
        }
    } else if !report.refusals.is_empty() {
        out.push_str(
            "\nno debug layer messages in this log: a run with VICTORY_D3D12_DEBUG=1 says why each \
             pipeline was refused\n",
        );
    }
    if !report.untranslated.is_empty() {
        out.push_str(&format!("\nshaders that do not translate ({} kinds)\n", report.untranslated.len()));
        for (shader, count) in sorted(&report.untranslated) {
            out.push_str(&format!(
                "  {count:>5}x stage={} hash={} {}\n",
                shader.stage, shader.hash, shader.error
            ));
        }
    }
    if !report.drops.is_empty() {
        out.push_str("\ndropped a frame for want of a host pipeline, by reason\n");
        let mut drops: Vec<(&String, &f64)> = report.drops.iter().collect();
        drops.sort_by(|a, b| b.1.total_cmp(a.1));
        for (why, most) in drops {
            out.push_str(&format!("  {most:>7.1}/frame at most: {why}\n"));
        }
    }
    out
}

fn sorted<K: Ord>(map: &BTreeMap<K, u64>) -> Vec<(&K, u64)> {
    let mut items: Vec<(&K, u64)> = map.iter().map(|(k, &v)| (k, v)).collect();
    items.sort_by_key(|a| std::cmp::Reverse(a.1));
    items
}

/// `cargo xtask refusals <log>`.
pub(crate) fn run(log: &Path) -> Result<()> {
    let text = std::fs::read_to_string(log)
        .with_context(|| format!("reading the run's log {}", log.display()))?;
    print(&parse(&text));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines shaped as `victory-gpu` writes them: the refusal lines carry
    /// `?shaders` (the root signature's too), a shader that does not translate
    /// carries `error=` and then `cached=`, `ms=`, `user_ms=`, `kernel_ms=`,
    /// and the debug layer's messages carry their id and severity.
    const LOG: &str = "\
2026-11-25T20:23:59.901311Z  WARN victory_gpu::pipeline::host: host pipeline refused error=参数错误。 (0x80070057) shaders=[compute 2da4beb386509b77]
2026-11-25T20:23:59.926311Z  WARN victory_gpu::pipeline::host: host pipeline refused error=参数错误。 (0x80070057) shaders=[compute 2da4beb386509b77]
2026-11-25T20:25:04.143453Z  WARN victory_gpu::pipeline::host: host mesh pipeline refused error=参数错误。 (0x80070057) shaders=[mesh f5e50eb2108c425b, pixel ce3f0432a08f1d35]
2026-11-25T20:23:58.000000Z  WARN victory_gpu::pipeline::host: host root signature refused error=参数错误。 (0x80070057) shaders=[compute aaaaaaaaaaaaaaaa]
2026-11-25T20:23:59.901176Z  WARN victory_gpu::pipeline: shader does not translate; its pipelines' draws are dropped stage=\"compute\" hash=926c46bab3b86bd6 error=converter shader.dxil v1: GFX10 code at 0x13c8: a lane mask holding lanes elsewhere read as data is not supported cached=false ms=12 user_ms=11 kernel_ms=1
2026-11-25T20:23:59.902176Z  WARN victory_gpu::pipeline: shader does not translate; its pipelines' draws are dropped stage=\"compute\" hash=926c46bab3b86bd6 error=converter shader.dxil v1: GFX10 code at 0x13c8: a lane mask holding lanes elsewhere read as data is not supported cached=false ms=31 user_ms=29 kernel_ms=2
2026-11-25T20:23:59.266134Z  WARN victory_gpu::host: D3D12: ID3D12Device::CreateDescriptorHeap: NumDescriptors 2097152, but the limit is 1000000 id=645 severity=1
2026-11-25T20:23:59.901311Z  WARN victory_gpu::host: D3D12: ID3D12Device::CreateComputeShader: Shader wants wave lane count of 64 but maximum is 32 id=622 severity=1
2026-11-25T20:23:59.901311Z  WARN victory_gpu::host: D3D12: ID3D12Device::CreateComputeShader: Shader wants wave lane count of 64 but maximum is 32 id=622 severity=1
2026-11-25T20:41:49.807123Z  INFO victory_gpu::host: dropped 227.7/frame without a host pipeline: a stage of the ray-tracing structure builder, which is off
2026-11-25T20:41:49.807123Z  INFO victory_gpu::host: frame line that is not any of these
";

    #[test]
    fn the_report_groups_what_a_log_says() {
        let report = parse(LOG);
        assert_eq!(report.refusals.len(), 3, "the compute pipeline twice is one kind, plus the mesh and root signature ones");
        let (compute, count) = report
            .refusals
            .iter()
            .find(|(r, _)| r.kind == "pipeline")
            .expect("the compute refusal is there");
        assert_eq!(*count, 2);
        assert_eq!(compute.shaders, "compute 2da4beb386509b77");
        assert!(compute.error.contains("0x80070057"));
        // Two runs of the same shader with different timings are one kind: the
        // error stops at ` cached=`, so `ms=` and the rest never reach the key.
        assert_eq!(report.untranslated.len(), 1);
        let (untranslated, runs) = report.untranslated.iter().next().unwrap();
        assert_eq!(*runs, 2);
        assert_eq!(untranslated.stage, "compute");
        assert_eq!(untranslated.hash, "926c46bab3b86bd6");
        assert!(untranslated.error.contains("converter shader.dxil"));
        assert!(!untranslated.error.contains("cached="), "the timings are not part of the shader: {}", untranslated.error);
        assert_eq!(report.debug.len(), 2);
        let wave = report
            .debug
            .iter()
            .find(|(m, _)| m.text.contains("wave lane count"))
            .expect("the wave64 message is there");
        assert_eq!(*wave.1, 2);
        assert_eq!(wave.0.id, 622);
        // The root signature's line carries shaders too, so it is read like any
        // other refusal.
        let (root, _) = report
            .refusals
            .iter()
            .find(|(r, _)| r.kind == "root signature")
            .expect("the root signature refusal is there");
        assert_eq!(root.shaders, "compute aaaaaaaaaaaaaaaa");
        // The drop rate is read off the line, not left at zero.
        assert_eq!(report.drops.get("a stage of the ray-tracing structure builder, which is off"), Some(&227.7));
    }

    #[test]
    fn a_log_without_reasons_says_what_to_turn_on() {
        let report = parse("host pipeline refused error=0x80070057 shaders=[compute abc]\n");
        assert_eq!(report.refusals.len(), 1);
        assert!(report.debug.is_empty());
        // What the tool is for: a reader who sees refusals and no reasons must
        // be told which switch gives them.
        let text = render(&report);
        assert!(text.contains("VICTORY_D3D12_DEBUG=1"), "the hint a reader needs: {text}");
    }

    #[test]
    fn a_log_with_nothing_in_it_says_so() {
        let text = render(&parse("2026-11-25T20:41:49.807123Z  INFO victory_gpu::host: a frame line\n"));
        assert_eq!(text, "nothing refused, dropped, or untranslated in this log\n");
    }
}
