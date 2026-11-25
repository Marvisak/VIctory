//! `victory doctor`: the failures of a run, each with a link that searches
//! the project's issues and pull requests for it, and an issue draft.
//!
//! It reads the run's log (what `run --execute` writes to stderr) and the
//! entries `crashes.log` gained during that run. It goes on the network for
//! nothing: the links are for the person to open. Drafts have the user name
//! and home folder taken out of every path.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One kind of failure in a run, as reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// Most severe first: 0 crash, 1 device lost or commit, 2 error, 3 warning.
    pub severity: u8,
    /// Stable across runs and builds; what failures are counted under.
    pub key: String,
    pub title: String,
    /// What to search the issues for.
    pub terms: Vec<String>,
    pub count: usize,
    /// The lines around the first one, user names taken out.
    pub excerpt: Vec<String>,
    /// Shader hashes the messages name.
    pub shaders: Vec<String>,
}

/// Messages that are expected in every run.
const QUIET: [&str; 3] = [
    "VICTORY_SHADER_LOOP_LIMIT",
    "a texture picked by index changed after its upload",
    "levels after the black frame",
];

fn exception_name(code: &str) -> &'static str {
    match code {
        "0xc0000005" => "access violation",
        "0x80000003" => "breakpoint (the title stopped itself)",
        "0xc000001d" => "illegal instruction (the CPU lacks one the title uses)",
        "0xc0000409" => "stack buffer overrun",
        "0xc00000fd" => "stack overflow",
        "0x887a0001" => "DXGI_ERROR_INVALID_CALL",
        "0x887a0005" => "DXGI_ERROR_DEVICE_REMOVED",
        "0x887a0006" => "DXGI_ERROR_DEVICE_HUNG",
        "0x887a0007" => "DXGI_ERROR_DEVICE_RESET",
        _ => "exception",
    }
}

/// `line` without terminal colour codes.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_hex16(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The module of a crash frame: `image` for any title image (their names
/// carry a hash that differs between builds).
fn module(name: &str) -> &str {
    if name.starts_with("victory-image-") {
        "image"
    } else {
        name
    }
}

/// `0x...` at the start of `s`, lower-cased.
fn hex_at(s: &str) -> Option<String> {
    let rest = s.strip_prefix("0x")?;
    let n = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
    (n > 0).then(|| format!("0x{}", rest[..n].to_ascii_lowercase()))
}

/// `module+0x...` in a crash line or frame.
fn site(s: &str) -> Option<(String, String)> {
    let plus = s.find("+0x")?;
    let name = s[..plus].rsplit(|c: char| c.is_whitespace()).next()?;
    Some((module(name).to_owned(), hex_at(&s[plus + 1..])?))
}

/// The words of a message before its first `key=value`, number or bracket.
fn phrase(message: &str) -> String {
    let mut words = Vec::new();
    for w in message.split_whitespace() {
        if w.contains('=')
            || w.starts_with('[')
            || w.starts_with('(')
            || w.bytes().any(|b| b.is_ascii_digit())
        {
            break;
        }
        words.push(w.trim_matches(|c| matches!(c, ':' | ';' | ',' | '.')));
        if words.len() == 9 {
            break;
        }
    }
    words.join(" ")
}

/// `slot=`, `format=` and the like: what tells two messages of one kind apart.
fn fields(message: &str) -> String {
    message
        .split_whitespace()
        .filter_map(|w| w.split_once('='))
        .filter(|(k, _)| {
            matches!(
                *k,
                "slot" | "format" | "stage" | "dimension" | "kind" | "dll"
            )
        })
        .map(|(k, v)| format!("{k}={}", v.trim_end_matches(',')))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Shader hashes a message names (`hash=...`, `[compute ...]`).
fn shaders(message: &str) -> Vec<String> {
    let mut out = Vec::new();
    let words: Vec<&str> = message.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        let w = w.trim_matches(|c| matches!(c, '[' | ']' | ',' | '"'));
        if let Some(h) = w.strip_prefix("hash=") {
            let h = h.trim_matches('"');
            if is_hex16(h) {
                out.push(h.to_owned());
            }
        } else if is_hex16(w)
            && i > 0
            && matches!(
                words[i - 1].rsplit(['[', '=']).next().unwrap_or_default(),
                "compute" | "mesh" | "pixel" | "vertex" | "geometry" | "hull" | "domain"
            )
        {
            out.push(w.to_owned());
        }
    }
    out
}

/// Takes the user name and home folder out of paths.
pub fn redact(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.to_ascii_lowercase().find(":\\users\\") {
        let end = at + ":\\users\\".len();
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        let name = rest.find(['\\', '/', ' ', '"']).unwrap_or(rest.len());
        out.push_str("<user>");
        rest = &rest[name..];
    }
    out.push_str(rest);
    out
}

/// The failures in `lines` (a run's log, then its crash entries).
pub fn failures<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Failure> {
    let lines: Vec<String> = lines.into_iter().map(plain).collect();
    let mut found: BTreeMap<String, Failure> = BTreeMap::new();
    let add = |found: &mut BTreeMap<String, Failure>, i: usize, f: Failure| {
        let e = found.entry(f.key.clone()).or_insert_with(|| Failure {
            excerpt: lines[i.saturating_sub(2)..(i + 8).min(lines.len())]
                .iter()
                .map(|l| redact(l))
                .collect(),
            count: 0,
            ..f.clone()
        });
        e.count += 1;
        for s in f.shaders {
            if !e.shaders.contains(&s) && e.shaders.len() < 12 {
                e.shaders.push(s);
            }
        }
    };
    for (i, line) in lines.iter().enumerate() {
        if let Some(at) = line.find("unhandled exception 0x") {
            let rest = &line[at + "unhandled exception ".len()..];
            let Some(code) = hex_at(rest) else { continue };
            let Some((m, off)) = site(rest) else { continue };
            let mut where_ = format!("{m}+{off}");
            let mut terms = vec![off.clone()];
            if m != "image" {
                // Our own code: its offsets change with every build; the
                // title's frame below it does not.
                terms.clear();
                if let Some((_, toff)) = lines[i + 1..(i + 14).min(lines.len())]
                    .iter()
                    .filter_map(|l| l.trim().strip_prefix('#').and_then(site))
                    .find(|(m, _)| m == "image")
                {
                    write!(where_, " (title frame image+{toff})").ok();
                    terms.push(toff);
                }
                if terms.is_empty() {
                    terms.push(code.clone());
                }
            }
            let access = line[at..]
                .split('(')
                .nth(1)
                .map(|a| a.split(':').next().unwrap_or(a).trim_end_matches(')'));
            let title = format!(
                "Crash: {} at {where_}{}",
                exception_name(&code),
                access.map(|a| format!(" ({a})")).unwrap_or_default()
            );
            let key = format!("crash {code} at {where_}");
            add(
                &mut found,
                i,
                Failure {
                    severity: 0,
                    key,
                    title,
                    terms,
                    count: 0,
                    excerpt: vec![],
                    shaders: vec![],
                },
            );
            continue;
        }
        if line.contains("os error 1455") {
            let f = Failure {
                severity: 1,
                key: "os error 1455".into(),
                title: "Windows commit limit reached (os error 1455): close programs or raise the paging file".into(),
                terms: vec!["1455".into()],
                count: 0,
                excerpt: vec![],
                shaders: vec![],
            };
            add(&mut found, i, f);
            continue;
        }
        // `<time>  WARN <module>: <message>`
        let mut parts = line.split_whitespace();
        let (Some(_), Some(level), Some(module)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !matches!(level, "WARN" | "ERROR") || !module.ends_with(':') {
            continue;
        }
        let message = line
            .split_once(module)
            .map(|(_, m)| m.trim())
            .unwrap_or_default();
        if QUIET.iter().any(|q| message.contains(q)) {
            continue;
        }
        let lower = message.to_ascii_lowercase();
        if let Some(at) = lower.find("0x887a000")
            && (lower.contains("removed") || lower.contains("hung") || lower.contains("device"))
        {
            // A line cut right after the prefix, or a wide character there,
            // is not a code.
            let Some(code) = lower
                .get(at..at + 10)
                .filter(|c| c.as_bytes()[9].is_ascii_hexdigit())
            else {
                continue;
            };
            let code = code.to_owned();
            let f = Failure {
                severity: 1,
                key: format!("device {code}"),
                title: format!("GPU device lost: {}", exception_name(&code)),
                terms: vec![code],
                count: 0,
                excerpt: vec![],
                shaders: vec![],
            };
            add(&mut found, i, f);
            continue;
        }
        let words = phrase(message);
        let fields = fields(message);
        let with = |s: &str| {
            if fields.is_empty() {
                s.to_owned()
            } else {
                format!("{s} ({fields})")
            }
        };
        let f = Failure {
            severity: if level == "ERROR" { 2 } else { 3 },
            key: with(&format!(
                "{} {}",
                module.trim_end_matches(':'),
                if words.is_empty() { message } else { &words }
            )),
            title: with(&format!(
                "{}: {}",
                if level == "ERROR" { "Error" } else { "Warning" },
                if words.is_empty() { message } else { &words }
            )),
            terms: if words.split(' ').count() >= 3 {
                vec![format!("\"{words}\"")]
            } else {
                vec![]
            },
            count: 0,
            excerpt: vec![],
            shaders: shaders(message),
        };
        add(&mut found, i, f);
    }
    let mut out: Vec<Failure> = found.into_values().collect();
    out.sort_by(|a, b| a.severity.cmp(&b.severity).then(b.count.cmp(&a.count)));
    out
}

/// Percent-encoding for a URL query value.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => write!(out, "%{b:02X}").unwrap_or(()),
        }
    }
    out
}

/// A link that searches `repo`'s issues and pull requests for a failure.
pub fn search_link(repo: &str, f: &Failure) -> Option<String> {
    let mut terms = f.terms.clone();
    if terms.is_empty() {
        terms.extend(f.shaders.first().cloned());
    }
    (!terms.is_empty()).then(|| {
        format!(
            "https://github.com/search?type=issues&q={}",
            encode(&format!("repo:{repo} {}", terms.join(" ")))
        )
    })
}

/// Seconds since 1970 of `YYYY-MM-DDTHH:MM:SS` (UTC).
fn unix(stamp: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| stamp.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, min, s) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + min * 60 + s)
}

/// The entries of `crashes.log` text written between `from` and `to`.
fn crash_entries(text: &str, from: i64, to: i64) -> Vec<&str> {
    let mut out = Vec::new();
    let mut keep = false;
    for line in text.lines() {
        if let Some(t) = line.strip_prefix("unix time ") {
            let t = t
                .split(',')
                .next()
                .and_then(|t| t.trim().parse::<i64>().ok())
                .unwrap_or(0);
            keep = (from..=to).contains(&t);
        }
        if keep {
            out.push(line);
        }
    }
    out
}

/// Every `crashes.log` under the data folder.
fn crash_logs() -> Vec<PathBuf> {
    let Ok(cfg) = victory_core::Config::load() else {
        return Vec::new();
    };
    let Ok(dirs) = std::fs::read_dir(&cfg.data_dir) else {
        return Vec::new();
    };
    dirs.flatten()
        .map(|d| d.path().join("crashes.log"))
        .filter(|p| p.is_file())
        .collect()
}

/// `s` as a JSON string.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap_or(()),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_list(items: &[String]) -> String {
    let v: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", v.join(","))
}

/// The failures as JSON, one object each: for scripts and agents that
/// triage run reports.
pub fn to_json(repo: &str, log: &Path, found: &[Failure]) -> String {
    let items: Vec<String> = found
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let kind = ["crash", "fatal", "error", "warning"][usize::from(f.severity.min(3))];
            let link = search_link(repo, f).map_or("null".to_owned(), |l| json_str(&l));
            format!(
                "{{\"number\":{},\"kind\":\"{kind}\",\"key\":{},\"title\":{},\"count\":{},\"search_terms\":{},\"shaders\":{},\"search_url\":{link},\"excerpt\":{}}}",
                i + 1,
                json_str(&f.key),
                json_str(&f.title),
                f.count,
                json_list(&f.terms),
                json_list(&f.shaders),
                json_list(&f.excerpt)
            )
        })
        .collect();
    format!(
        "{{\"log\":{},\"repo\":{},\"failures\":[{}]}}\n",
        json_str(&redact(&log.display().to_string())),
        json_str(repo),
        items.join(",")
    )
}

pub fn run(
    log: &Path,
    crashes: Option<PathBuf>,
    draft: Option<usize>,
    repo: &str,
    json: bool,
) -> Result<()> {
    let text =
        // Bytes, not UTF-8: a crash can cut a log mid-character.
        std::fs::read(log)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .with_context(|| format!("reading {}", log.display()))?;
    let stamps: Vec<i64> = text
        .lines()
        .filter_map(|l| unix(plain(l).get(..19)?))
        .collect();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let paths = crashes.map(|c| vec![c]).unwrap_or_else(crash_logs);
    if let (Some(&from), Some(&to)) = (stamps.first(), stamps.last()) {
        for p in paths {
            if let Ok(c) = std::fs::read(&p).map(|b| String::from_utf8_lossy(&b).into_owned()) {
                lines.extend(
                    crash_entries(&c, from, to + 60)
                        .into_iter()
                        .map(str::to_owned),
                );
            }
        }
    }
    let found = failures(lines.iter().map(String::as_str));
    if json {
        print!("{}", to_json(repo, log, &found));
        return Ok(());
    }
    if let Some(n) = draft {
        let f = found
            .get(n.wrapping_sub(1))
            .with_context(|| format!("no failure {n} (1-{})", found.len()))?;
        print!("{}", issue_draft(repo, f));
        return Ok(());
    }
    if found.is_empty() {
        println!("No failures in {}.", log.display());
        return Ok(());
    }
    println!(
        "{} kinds of failure, most severe first. Open a link to see who else reported it.\n",
        found.len()
    );
    for (i, f) in found.iter().enumerate() {
        println!("[{}] {}  (x{})", i + 1, f.title, f.count);
        if !f.shaders.is_empty() {
            println!(
                "    shaders: {}",
                f.shaders
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(link) = search_link(repo, f) {
            println!("    {link}");
        }
    }
    println!(
        "\nNothing reported yet? `victory doctor {} --draft <n>` writes the issue.",
        log.display()
    );
    Ok(())
}

/// A failure's title as a draft may carry it: a crash keeps its code,
/// module-relative offsets and the kind of access, not the address it
/// touched (only whether it was null). Messages are our own text.
fn public_title(f: &Failure) -> String {
    if f.severity != 0 {
        return f.title.clone();
    }
    for access in ["read", "write", "execute"] {
        let mark = format!(" ({access} 0x");
        if let Some(at) = f.title.rfind(&mark) {
            let null = f.title[at + mark.len()..].starts_with("0)");
            let what = if null {
                format!(" ({access} at address 0)")
            } else {
                format!(" ({access})")
            };
            return format!("{}{what}", &f.title[..at]);
        }
    }
    f.title.clone()
}

/// The paste-ready issue text. It carries an allowlist of fields only: what
/// kind of failure, its title (our message text, or a crash's code and
/// module-relative offsets), how often, and a search made of the same. Log
/// lines and shader hashes stay in the local output: lines near a failure
/// can hold values read from title memory (`VICTORY_PEEK`), and title
/// shader hashes do not go in comments (rule 7 of #1633).
fn issue_draft(repo: &str, f: &Failure) -> String {
    let kind = ["crash", "fatal", "error", "warning"][usize::from(f.severity.min(3))];
    let title = public_title(f);
    let mut s = String::new();
    writeln!(s, "Title: {title}\n").ok();
    writeln!(
        s,
        "### What happened\n\n- Kind: {kind}\n- Failure: {title}\n- Times in one run: {}\n",
        f.count
    )
    .ok();
    writeln!(s, "### Environment\n\n- Windows version:\n- GPU and driver:\n- CPU, RAM:\n- VIctory commit:\n- Console mode and `VICTORY_*` settings:\n").ok();
    if !f.terms.is_empty() {
        let terms = Failure {
            shaders: Vec::new(),
            ..f.clone()
        };
        if let Some(link) = search_link(repo, &terms) {
            writeln!(s, "Searched first: {link}\n").ok();
        }
    }
    writeln!(
        s,
        "Written by `victory doctor --draft`: no log lines, shader hashes or memory values."
    )
    .ok();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "\
2026-11-25T10:23:56.997492Z  WARN victory_gpu::descriptor::host: host view heap capped at 1000000; views past it read a null slot count=1245184
2026-11-25T10:24:00.000000Z  WARN victory_gpu::pipeline::host: host pipeline refused error=The parameter is incorrect. (0x80070057) shaders=[compute 6f225b641c5b0708]
2026-11-25T10:24:01.000000Z  WARN victory_gpu::pipeline::host: host pipeline refused error=The parameter is incorrect. (0x80070057) shaders=[compute 2da4beb386509b77]
2026-11-25T10:24:02.000000Z  WARN victory_gpu::device: a pipeline creation method that is not implemented: the title goes without this pipeline slot=0x1c8 calls=1
2026-11-25T10:24:02.000000Z  WARN victory_gpu::device: a pipeline creation method that is not implemented: the title goes without this pipeline slot=0x218 calls=1
2026-11-25T10:24:03.000000Z  WARN victory_gpu::shader_cache: VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner limit=32
2026-11-25T10:24:04.000000Z  INFO victory_gpu::host: 3.1 fps, 319.0 ms/frame
";

    const CRASHES: &str = "\
unix time 1795602000, process 1
VIctory: unhandled exception 0x80000003 at victory-image-0123456789abcdef.dll+0x1e6c29f
  #0  victory-image-0123456789abcdef.dll+0x1e6c29f

unix time 1795602452, process 9072
VIctory: unhandled exception 0xc0000005 at victory-image-b99f876b74d4a77f.dll+0x23cf683 (write 0x0: nothing mapped there)
  #0  victory-image-b99f876b74d4a77f.dll+0x23cf683
unix time 1795602460, process 9073
VIctory: unhandled exception 0xc0000005 at VCRUNTIME140.dll+0x1dafb (read 0xb5bb40000: reserved, never committed)
  #0  VCRUNTIME140.dll+0x1dafb
  #6  victory-image-b99f876b74d4a77f.dll+0x24821e3
VIctory: Windows commit limit reached while committing 6538919936 bytes (os error 1455).
";

    #[test]
    fn a_run_s_failures_are_counted_by_kind_and_ordered_by_severity() {
        let found = failures(RUN.lines().chain(CRASHES.lines()));
        let keys: Vec<&str> = found.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(found[0].severity, 0);
        assert!(
            keys.contains(&"crash 0xc0000005 at image+0x23cf683"),
            "{keys:?}"
        );
        assert!(
            keys.contains(
                &"crash 0xc0000005 at VCRUNTIME140.dll+0x1dafb (title frame image+0x24821e3)"
            ),
            "{keys:?}"
        );
        assert!(keys.contains(&"os error 1455"));
        let refused = found
            .iter()
            .find(|f| f.key.contains("host pipeline refused"))
            .unwrap();
        assert_eq!(refused.count, 2, "one kind, counted twice");
        assert_eq!(refused.shaders, ["6f225b641c5b0708", "2da4beb386509b77"]);
        assert_eq!(
            found
                .iter()
                .filter(|f| f.key.contains("not implemented"))
                .count(),
            2,
            "told apart by slot"
        );
        assert!(
            !keys.iter().any(|k| k.contains("LOOP_LIMIT")),
            "expected messages are left out"
        );
        let ours = found.iter().find(|f| f.key.contains("VCRUNTIME")).unwrap();
        assert_eq!(
            ours.terms,
            ["0x24821e3"],
            "our own code is searched by the title's frame"
        );
        let heap = found
            .iter()
            .find(|f| f.key.contains("view heap capped"))
            .unwrap();
        assert_eq!(heap.terms, ["\"host view heap capped at\""]);
    }

    #[test]
    fn only_the_run_s_own_crash_entries_are_read() {
        let from = unix("2026-11-25T10:27:00").unwrap();
        assert_eq!(unix("1970-01-01T00:00:00"), Some(0));
        assert_eq!(unix("2000-03-01T00:00:01"), Some(951_868_801));
        let entries = crash_entries(CRASHES, from, from + 600);
        assert!(entries.iter().any(|l| l.contains("0x23cf683")));
        assert!(
            !entries.iter().any(|l| l.contains("0x1e6c29f")),
            "an earlier run's crash"
        );
    }

    #[test]
    fn links_and_drafts_name_no_user() {
        let f = failures(CRASHES.lines()).remove(0);
        let link = search_link("Marvisak/VIctory", &f).unwrap();
        assert!(
            link.starts_with(
                "https://github.com/search?type=issues&q=repo%3AMarvisak%2FVIctory+0x"
            ),
            "{link}"
        );
        assert_eq!(
            redact(r"C:\Users\someone\AppData\x and D:\users\other/y"),
            r"C:\Users\<user>\AppData\x and D:\users\<user>/y"
        );
        assert!(
            issue_draft("Marvisak/VIctory", &f).contains("Written by `victory doctor --draft`")
        );
    }

    /// A shader hash and a peek line next to a failure (title-derived data)
    /// reach neither the draft nor the page's filled-in issue form.
    #[test]
    fn drafts_carry_no_log_lines_hashes_or_memory_values() {
        let log = "\
2026-11-25T10:24:00.000000Z  INFO victory_gpu::capture: peek frame 0x1a2b3c4d00: 11223344 55667788 99aabbcc ddeeff00
2026-11-25T10:24:00.100000Z  WARN victory_gpu::pipeline::host: host pipeline refused error=The parameter is incorrect. (0x80070057) shaders=[compute 6f225b641c5b0708]
unix time 1795602452, process 9072
VIctory: unhandled exception 0xc0000005 at victory-image-b99f876b74d4a77f.dll+0x23cf683 (write 0x7ffe12345678: nothing mapped there)
  #0  victory-image-b99f876b74d4a77f.dll+0x23cf683
";
        let found = failures(log.lines());
        assert!(
            found
                .iter()
                .any(|f| f.shaders.contains(&"6f225b641c5b0708".to_owned())),
            "kept for local output"
        );
        for f in &found {
            let draft = issue_draft("Marvisak/VIctory", f);
            for secret in [
                "6f225b641c5b0708",
                "11223344",
                "0x1a2b3c4d00",
                "0x7ffe12345678",
                "peek",
            ] {
                assert!(!draft.contains(secret), "{secret} in the draft:\n{draft}");
            }
        }
        let crash = found.iter().find(|f| f.severity == 0).unwrap();
        assert!(issue_draft("Marvisak/VIctory", crash).contains("image+0x23cf683 (write)"));
    }

    #[test]
    fn cut_device_codes_and_invalid_bytes_do_not_panic() {
        let lines = [
            "2026-11-25T10:24:00.000000Z  WARN victory_gpu::host: the device was removed 0x887a000",
            "2026-11-25T10:24:00.000000Z  WARN victory_gpu::host: the device was removed 0x887a000é",
        ];
        let found = failures(lines.iter().copied());
        assert!(found.iter().all(|f| !f.key.starts_with("device ")));
        let dir = std::env::temp_dir().join("victory-doctor-bytes");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("cut.log");
        let mut bytes = b"2026-11-25T10:24:00.000000Z  WARN victory_gpu::host: refused ".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        std::fs::write(&log, bytes).unwrap();
        let none = dir.join("none.log");
        assert!(run(&log, Some(none), None, "Marvisak/VIctory", true).is_ok());
    }

    #[test]
    fn json_output_is_escaped_and_complete() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), r#""a\"b\\c\nd\u0001""#);
        let found = failures(RUN.lines().chain(CRASHES.lines()));
        let out = to_json(
            "Marvisak/VIctory",
            Path::new(r"C:\Users\someone\run.log"),
            &found,
        );
        assert!(out.starts_with(r#"{"log":"C:\\Users\\<user>\\run.log","repo":"Marvisak/VIctory","failures":[{"number":1,"kind":"crash""#), "{out}");
        assert_eq!(out.matches("\"number\":").count(), found.len());
        assert!(out.contains(r#""shaders":["6f225b641c5b0708","2da4beb386509b77"]"#));
        assert!(out.trim_end().ends_with("]}"));
    }
}
