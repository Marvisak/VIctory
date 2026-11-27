//! Checks that every `VICTORY_*` switch named in the docs is read by Rust
//! source. Doc text drifts easily: a switch can be renamed or removed while
//! its README row stays. This is a textual check, not a build-configuration
//! proof: a read behind `cfg!(windows)` or `cfg!(feature = "diagnostics")` is
//! still a read.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

/// Documents and Rust sources checked by [`run`].
const DOC_ROOT: &str = "docs";
const DOC_README: &str = "README.md";
const RUST_ROOTS: &[&str] = &["apps", "crates", "xtask", "tests"];

pub fn run(root: &Path) -> Result<()> {
    let mut documents = Vec::new();
    let mut sources = Vec::new();

    for (relative, path) in files(root, DOC_ROOT, "md")
        .into_iter()
        .map(|p| (p.clone(), root.join(p)))
        .chain(std::iter::once((
            DOC_README.to_owned(),
            root.join(DOC_README),
        )))
    {
        documents.push((
            relative,
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?,
        ));
    }
    for rust_root in RUST_ROOTS {
        for relative in files(root, rust_root, "rs") {
            let path = root.join(&relative);
            sources.push((
                relative,
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            ));
        }
    }

    let violations = check(&documents, &sources);
    if violations.is_empty() {
        println!("switch docs: {} names ok", documented(&documents).len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("SWITCH DOCS: {v}");
    }
    bail!("{} switch-documentation violation(s)", violations.len())
}

/// Files under `root/<dir>` with `extension`, relative to `root`, never
/// generated output.
fn files(root: &Path, dir: &str, extension: &str) -> Vec<String> {
    WalkDir::new(root.join(dir))
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != "target" && name != ".git"
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|x| x == extension))
        .map(|e| {
            e.path()
                .strip_prefix(root)
                .unwrap_or(e.path())
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

/// One diagnostic per documented name with no read in any source.
pub fn check(documents: &[(String, String)], sources: &[(String, String)]) -> Vec<String> {
    let reads: HashSet<String> = sources
        .iter()
        .flat_map(|(_, source)| rust_reads(source))
        .collect();
    let mut seen = HashSet::new();
    let mut violations = Vec::new();
    for (path, text) in documents {
        for (line, name) in documented_names(text) {
            if reads.contains(&name) || !seen.insert(name.clone()) {
                continue;
            }
            violations.push(format!("{path}:{line}: {name} is not read in Rust source"));
        }
    }
    violations
}

/// Every distinct `VICTORY_*` name named in the documents.
fn documented(documents: &[(String, String)]) -> HashSet<String> {
    documents
        .iter()
        .flat_map(|(_, text)| documented_names(text))
        .map(|(_, name)| name)
        .collect()
}

/// The exact names in one document, with one-based line numbers.
fn documented_names(text: &str) -> Vec<(usize, String)> {
    const PREFIX: &str = "VICTORY_";
    let mut names = Vec::new();
    for (line, text) in text.lines().enumerate() {
        let bytes = text.as_bytes();
        let mut at = 0;
        while let Some(rest) = text[at..].find(PREFIX) {
            let start = at + rest;
            let mut end = start + PREFIX.len();
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase()
                    || bytes[end].is_ascii_digit()
                    || bytes[end] == b'_')
            {
                end += 1;
            }
            if end > start + PREFIX.len() {
                names.push((line + 1, text[start..end].to_owned()));
            }
            at = end.max(start + PREFIX.len());
        }
    }
    names
}

/// Names passed to a read expression, excluding comments and writes.
fn rust_reads(source: &str) -> HashSet<String> {
    const PREFIX: &str = "VICTORY_";
    let text = mask_comments(source);
    let bytes = text.as_bytes();
    let mut reads = HashSet::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(rel) = text[at..].find('"') else {
            break;
        };
        let start = at + rel;
        let mut end = start + 1;
        while end < bytes.len() && bytes[end] != b'"' {
            if bytes[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        let name = &text[start + 1..end];
        if name.starts_with(PREFIX)
            && name[PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && is_read(&text[..start])
        {
            reads.insert(name.to_owned());
        }
        at = end + 1;
    }
    reads
}

/// Whether a string literal at this position is an environment read. Writes
/// (`.env("NAME", ...)` and `set_var`) are deliberately not reads.
fn is_read(prefix: &str) -> bool {
    let prefix = prefix.trim_end().trim_end_matches('(');
    if prefix.ends_with("set_var") || prefix.ends_with(".env") || prefix.ends_with("env_remove") {
        return false;
    }
    [
        "env!",
        "option_env!",
        "var",
        "var_os",
        "on",
        "env_path",
        "env",
        "numbers",
    ]
    .iter()
    .any(|call| prefix.ends_with(call))
}

/// Replaces comments with spaces, keeping string literals and line numbers.
fn mask_comments(source: &str) -> String {
    let mut out = source.as_bytes().to_vec();
    let mut at = 0;
    while at + 1 < out.len() {
        match (out[at], out[at + 1]) {
            (b'/', b'/') => {
                while at < out.len() && out[at] != b'\n' {
                    out[at] = b' ';
                    at += 1;
                }
            }
            (b'/', b'*') => {
                while at + 1 < out.len() && (out[at], out[at + 1]) != (b'*', b'/') {
                    if out[at] != b'\n' {
                        out[at] = b' ';
                    }
                    at += 1;
                }
                for byte in out.iter_mut().take(at + 2) {
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                }
                at += 2;
            }
            (b'"', _) => {
                // String literals stay readable: the switch names we look
                // for are in environment-read calls. Character literals and
                // lifetimes need no handling here.
                at += 1;
                while at < out.len() {
                    if out[at] == b'\\' {
                        at += 2;
                    } else if out[at] == b'"' {
                        at += 1;
                        break;
                    } else {
                        at += 1;
                    }
                }
            }
            _ => at += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| source.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{check, documented_names, mask_comments, rust_reads};

    #[test]
    fn direct_reads_are_found() {
        let source = r#"
            std::env::var("VICTORY_LOG");
            std::env::var_os("VICTORY_CAPTURE");
            crate::switch::on("VICTORY_D3D12_WARP");
            env!("VICTORY_SHADER_SOURCE_HASH");
            env_path("VICTORY_DATA_DIR");
            numbers("VICTORY_CAPTURE_WORLD");
        "#;
        let reads = rust_reads(source);
        for name in [
            "VICTORY_LOG",
            "VICTORY_CAPTURE",
            "VICTORY_D3D12_WARP",
            "VICTORY_SHADER_SOURCE_HASH",
            "VICTORY_DATA_DIR",
            "VICTORY_CAPTURE_WORLD",
        ] {
            assert!(reads.contains(name), "{name}");
        }
    }

    #[test]
    fn comments_and_writes_are_not_reads() {
        let source = r#"
            // VICTORY_FAKE
            /// VICTORY_ALSO_FAKE
            std::env::set_var("VICTORY_WRITTEN", "1");
            Command::new("x").env("VICTORY_CHILD", "1");
        "#;
        assert!(rust_reads(source).is_empty());
    }

    #[test]
    fn a_similar_name_does_not_credit_another() {
        let docs = vec![("README.md".into(), "VICTORY_WARP\n".into())];
        let sources = vec![(
            "crates/a/src/lib.rs".into(),
            "on(\"VICTORY_D3D12_WARP\");\n".into(),
        )];
        let violations = check(&docs, &sources);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("README.md:1: VICTORY_WARP"));
    }

    #[test]
    fn a_missing_switch_reports_its_document_line() {
        let docs = vec![("README.md".into(), "set VICTORY_FAKE=1\n".into())];
        let sources = vec![("crates/a/src/lib.rs".into(), "fn main() {}\n".into())];
        assert_eq!(
            check(&docs, &sources),
            vec!["README.md:1: VICTORY_FAKE is not read in Rust source"]
        );
    }

    #[test]
    fn duplicate_mentions_yield_one_violation() {
        let docs = vec![
            ("README.md".into(), "VICTORY_FAKE\nVICTORY_FAKE\n".into()),
            ("docs/x.md".into(), "VICTORY_FAKE\n".into()),
        ];
        assert_eq!(check(&docs, &[]).len(), 1);
    }

    #[test]
    fn document_scan_ignores_wildcards_and_lowercase() {
        let names = documented_names("VICTORY_* victory_fake VICTORY_REAL=1");
        assert_eq!(names, vec![(1, "VICTORY_REAL".to_owned())]);
    }

    #[test]
    fn block_comments_are_masked_without_moving_lines() {
        let masked = mask_comments("/* VICTORY_FAKE\n */\nvar(\"VICTORY_REAL\");");
        assert_eq!(masked.lines().count(), 3);
        assert!(!masked.contains("VICTORY_FAKE"));
        assert!(rust_reads(&masked).contains("VICTORY_REAL"));
    }
}
