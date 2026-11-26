//! Rule #1, statically: the repository only ever contains small text files
//! that we wrote. See docs/asset-policy.md.
//!
//! This is an allowlist, not a blocklist: a file passes only if its name is
//! on the list, it is not too big, it is text, and it does not smuggle data
//! inside text (long hex/base64 runs).

use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const MAX_FILE_SIZE: usize = 512 * 1024;
const READ_LIMIT: usize = MAX_FILE_SIZE + 1;
const MAX_ENCODED_RUN: usize = 512;

const ALLOWED_EXTENSIONS: &[&str] = &[
    "rs", "toml", "md", "txt", "lock", "yml", "yaml", "json", "hlsl", "hlsli", "ps1", "cmd", "sh",
];
const ALLOWED_NAMES: &[&str] = &[
    ".gitignore",
    ".gitattributes",
    ".editorconfig",
    ".victory-root",
    "pre-commit",
    "LICENSE",
    "LICENSE-MIT",
    "LICENSE-APACHE",
];

/// Paths explicitly exempted, with a reason. Keep this empty if at all possible.
const EXCEPTIONS: &[(&str, &str)] = &[(
    "crates/victory-formats/src/bcpack-tables.bin",
    "the BCPack format's constant tables: nothing decodes without them and they cannot be derived (owner's decision, 2026-11-23; docs/asset-policy.md)",
)];

/// Signatures of formats that must never be committed.
const FORBIDDEN_MAGIC: &[(&[u8], &str)] = &[
    (b"MZ", "PE executable"),
    (b"DXBC", "shader container"),
    (b"DDS ", "DDS texture"),
    (b"RIFF", "RIFF (audio/video)"),
    (b"BC\xC0\xDE", "LLVM bitcode"),
    (b"Microsoft C/C++ MSF", "PDB"),
    (b"!<arch>", "library archive"),
    (b"PK\x03\x04", "zip"),
    (b"7z\xBC\xAF", "7-zip"),
    (b"MSCF", "cabinet"),
    (b"\xD0\xCF\x11\xE0", "MSI/OLE"),
];

pub fn run(root: &Path, staged: bool) -> Result<()> {
    let files = list_files(root, staged)?;
    let mut violations = Vec::new();
    let mut buffer = vec![0; READ_LIMIT];
    for path in &files {
        if EXCEPTIONS.iter().any(|(p, _)| p == path) {
            continue;
        }
        match check_file(root, path, staged, &mut buffer) {
            Ok(problems) => {
                violations.extend(
                    problems
                        .into_iter()
                        .map(|problem| format!("{path}: {problem}")),
                );
            }
            Err(error) => violations.push(format!("{path}: {error:#}")),
        }
    }
    if violations.is_empty() {
        println!("asset policy: {} files ok", files.len());
        return Ok(());
    }
    for v in &violations {
        eprintln!("POLICY: {v}");
    }
    bail!(
        "{} asset-policy violation(s). Nothing from the game may be committed; \
         see docs/asset-policy.md",
        violations.len()
    )
}

fn check_file(root: &Path, path: &str, staged: bool, buffer: &mut [u8]) -> Result<Vec<String>> {
    let (content, total_len) = if staged {
        read_staged(root, path, buffer)?
    } else {
        let file = match File::open(root.join(path)) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("opening working-tree file"),
        };
        let total_len = file.metadata().context("reading file metadata")?.len();
        let content = read_prefix(file, buffer).context("reading working-tree file")?;
        (content, Some(total_len))
    };

    Ok(if content.len() > MAX_FILE_SIZE {
        check_sized(path, content, total_len)
    } else {
        check(path, content)
    })
}

fn read_staged<'a>(
    root: &Path,
    path: &str,
    buffer: &'a mut [u8],
) -> Result<(&'a [u8], Option<u64>)> {
    let object = format!(":{path}");
    let mut child = Command::new("git")
        .args(["cat-file", "blob", &object])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running git cat-file")?;
    let content = read_prefix(child.stdout.take().expect("piped git stdout"), buffer);
    // Close the pipe and stop git if there is nothing more we need to read.
    let oversized = matches!(&content, Ok(prefix) if prefix.len() > MAX_FILE_SIZE);
    if oversized || content.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("waiting for git cat-file")?;
    let content = content.context("reading staged blob")?;
    if !oversized {
        if !status.success() {
            bail!("reading staged blob failed (git cat-file {status})");
        }
        return Ok((content, None));
    }

    // The capped read already proved a violation. Try to report the exact size.
    let total_len = Command::new("git")
        .args(["cat-file", "-s", &object])
        .current_dir(root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok());
    Ok((content, total_len))
}

fn read_prefix(mut reader: impl Read, buffer: &mut [u8]) -> io::Result<&[u8]> {
    let mut len = 0;
    while len < buffer.len() {
        match reader.read(&mut buffer[len..]) {
            Ok(0) => break,
            Ok(read) => len += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(&buffer[..len])
}

fn list_files(root: &Path, staged: bool) -> Result<Vec<String>> {
    let git = |args: &[&str]| -> Result<Option<Vec<String>>> {
        let out = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .context("running git")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            out.stdout
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).replace('\\', "/"))
                .collect(),
        ))
    };
    if staged {
        return git(&[
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "-z",
        ])?
        .context("--staged needs a git repository");
    }
    if let Some(mut tracked) = git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])? {
        tracked.sort();
        tracked.dedup();
        return Ok(tracked);
    }
    // Not a git repository: walk everything except build output.
    Ok(walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("target" | ".git")))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect())
}

/// All problems with one file. Pure, for testing.
pub fn check(path: &str, content: &[u8]) -> Vec<String> {
    check_sized(path, content, Some(content.len() as u64))
}

fn check_sized(path: &str, content: &[u8], total_len: Option<u64>) -> Vec<String> {
    let mut problems = Vec::new();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let allowed = ALLOWED_NAMES.contains(&name)
        || ext
            .as_deref()
            .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e));
    if !allowed {
        problems.push("file type not on the allowlist".to_owned());
    }
    if content.len() > MAX_FILE_SIZE {
        let size = total_len.filter(|&len| len >= content.len() as u64);
        problems.push(match size {
            Some(len) => format!("{len} bytes exceeds the {MAX_FILE_SIZE}-byte limit"),
            None => format!("file size exceeds the {MAX_FILE_SIZE}-byte limit"),
        });
    }
    for (magic, what) in FORBIDDEN_MAGIC {
        if content.starts_with(magic) {
            problems.push(format!("looks like a {what}"));
        }
    }
    if content.contains(&0) {
        problems.push("binary content (NUL bytes)".to_owned());
    } else if let Some(run) = longest_encoded_run(content)
        && run > MAX_ENCODED_RUN
    {
        problems.push(format!("{run}-character hex/base64 run (embedded data?)"));
    } else {
        let digits = longest_hex_literal_run(content);
        if digits > MAX_ENCODED_RUN {
            problems.push(format!(
                "{digits} hex digits in one run of \\x/0x literals (embedded data?)"
            ));
        }
    }
    problems
}

/// Longest run of characters that could be hex or base64 payload.
fn longest_encoded_run(content: &[u8]) -> Option<usize> {
    let is_payload = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
    content.split(|b| !is_payload(b)).map(<[u8]>::len).max()
}

/// Most hex digits in `\xNN` escapes or `0x` literals with only commas,
/// whitespace or backslashes between them: a byte string or `xxd -i` array,
/// which `longest_encoded_run` sees as many 3- or 4-character runs.
/// shortcut: decimal arrays (`[77, 90, ...]`) still pass, add a literal count if one shows up.
fn longest_hex_literal_run(content: &[u8]) -> usize {
    let (mut i, mut run, mut longest) = (0, 0, 0);
    while i < content.len() {
        match &content[i..] {
            [b'\\' | b'0', b'x' | b'X', rest @ ..] => {
                let len = rest
                    .iter()
                    .take_while(|b| b.is_ascii_hexdigit() || **b == b'_')
                    .count();
                run += rest[..len].iter().filter(|b| b.is_ascii_hexdigit()).count();
                longest = longest.max(run);
                // Skip a type suffix such as `u8` or `u32`.
                let suffix = rest[len..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphanumeric())
                    .count();
                i += 2 + len + suffix;
            }
            [b, ..] if matches!(b, b',' | b'\\') || b.is_ascii_whitespace() => i += 1,
            _ => {
                run = 0;
                i += 1;
            }
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::{self, ErrorKind, Seek, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{MAX_FILE_SIZE, READ_LIMIT, check, check_file, read_prefix};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            let path = std::env::temp_dir().join(format!(
                "xtask-policy-{label}-{}-{}",
                std::process::id(),
                now.as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn worktree_reads_only_a_bounded_prefix_of_an_oversized_file() {
        let root = TestDir::new("oversized");
        let name = "oversized.bin";
        let path = root.0.join(name);
        let size = READ_LIMIT as u64 + 4096;
        let mut file = File::create(&path).unwrap();
        file.write_all(b"DDS \0").unwrap();
        file.set_len(size).unwrap();
        drop(file);

        let mut buffer = vec![0; READ_LIMIT];
        let mut file = File::open(&path).unwrap();
        assert_eq!(
            read_prefix(&mut file, &mut buffer).unwrap().len(),
            READ_LIMIT
        );
        assert_eq!(file.stream_position().unwrap(), READ_LIMIT as u64);
        assert_eq!(
            check_file(&root.0, name, false, &mut buffer).unwrap(),
            [
                "file type not on the allowlist".to_owned(),
                format!("{size} bytes exceeds the {MAX_FILE_SIZE}-byte limit"),
                "looks like a DDS texture".to_owned(),
                "binary content (NUL bytes)".to_owned(),
            ]
        );
    }

    #[test]
    fn worktree_skips_missing_files_and_surfaces_other_read_errors() {
        let root = TestDir::new("worktree-errors");
        let mut buffer = vec![0; READ_LIMIT];
        assert!(
            check_file(&root.0, "deleted.txt", false, &mut buffer)
                .unwrap()
                .is_empty()
        );

        fs::create_dir(root.0.join("directory.txt")).unwrap();
        let error = check_file(&root.0, "directory.txt", false, &mut buffer).unwrap_err();
        assert_ne!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn staged_reads_regular_blobs_and_rejects_gitlinks() {
        let root = TestDir::new("staged");
        let root = &root.0;
        git(
            root,
            &["init", "--bare", "--template=", "--object-format=sha1"],
        );
        fs::write(root.join("source.txt"), "a short note\n").unwrap();
        let blob = git(root, &["hash-object", "-w", "source.txt"]);
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},source.txt", blob.trim()),
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,link.txt",
            ],
        );

        let mut buffer = vec![0; READ_LIMIT];
        assert!(
            check_file(root, "source.txt", true, &mut buffer)
                .unwrap()
                .is_empty()
        );
        let error = check_file(root, "link.txt", true, &mut buffer).unwrap_err();
        assert!(error.to_string().contains("reading staged blob failed"));
        assert!(
            super::run(root, true)
                .unwrap_err()
                .to_string()
                .starts_with("1 asset-policy violation")
        );
    }

    #[test]
    fn staged_reads_only_a_bounded_prefix_of_an_oversized_blob() {
        let root = TestDir::new("staged-oversized");
        let root = &root.0;
        git(
            root,
            &["init", "--bare", "--template=", "--object-format=sha1"],
        );
        let size = READ_LIMIT + 4096;
        let mut bytes = vec![b'a'; size];
        bytes[..5].copy_from_slice(b"DDS \0");
        let src = root.join("oversized.bin");
        fs::write(&src, &bytes).unwrap();
        let blob = git(root, &["hash-object", "-w", "oversized.bin"]);
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{},oversized.bin", blob.trim()),
            ],
        );

        let mut buffer = vec![0; READ_LIMIT];
        assert_eq!(
            check_file(root, "oversized.bin", true, &mut buffer).unwrap(),
            [
                "file type not on the allowlist".to_owned(),
                format!("{size} bytes exceeds the {MAX_FILE_SIZE}-byte limit"),
                "looks like a DDS texture".to_owned(),
                "binary content (NUL bytes)".to_owned(),
            ]
        );
    }

    /// Enforces the policy wherever tests run, even in clones without the hook.
    #[test]
    fn repository_passes_policy() {
        super::run(&crate::workspace_root(), false).unwrap();
    }

    #[test]
    fn accepts_source() {
        assert!(check("crates/x/src/lib.rs", b"fn main() {}\n").is_empty());
        assert!(check(".gitignore", b"/target/\n").is_empty());
        assert!(check(".githooks/pre-commit", b"#!/bin/sh\n").is_empty());
    }

    #[test]
    fn rejects_game_like_files() {
        assert!(!check("assets/tex.dds", b"DDS \x7c\0\0\0").is_empty());
        assert!(
            !check("notes.txt", b"MZ\x90\0").is_empty(),
            "magic wins over extension"
        );
        assert!(
            !check("data.bin", b"hello").is_empty(),
            "extension not allowed"
        );
        assert!(
            !check("a.rs", &vec![b'a'; 600 * 1024]).is_empty(),
            "too big"
        );
    }

    #[test]
    fn rejects_data_smuggled_into_text() {
        let blob = format!("const DATA: &str = \"{}\";", "A1".repeat(400));
        assert!(!check("src/blob.rs", blob.as_bytes()).is_empty());
    }

    /// Synthetic bytes, never game data.
    fn synthetic(n: u32) -> impl Iterator<Item = u32> {
        (0..n).map(|i| (i * 37 + 11) % 256)
    }

    #[test]
    fn rejects_data_smuggled_as_an_escaped_byte_string() {
        // 4096 bytes as one unbroken `\xNN` hex run, the shape a byte-string
        // literal of copied data takes.
        let esc: String = synthetic(4096).map(|b| format!("\\x{b:02x}")).collect();
        let src = format!("pub const DATA: &[u8] = b\"{esc}\";\n");
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of \\x-escaped hex passed",
            src.len()
        );
    }

    #[test]
    fn rejects_data_smuggled_as_a_hex_byte_array() {
        // `xxd -i` style: 4096 bytes as hex literals.
        let lits: Vec<String> = synthetic(4096).map(|b| format!("0x{b:02x}")).collect();
        let src = format!("pub const DATA: [u8; 4096] = [{}];\n", lits.join(", "));
        assert!(
            !check("src/data.rs", src.as_bytes()).is_empty(),
            "{} bytes of hex array passed",
            src.len()
        );
        // A short constant table, like the 16-entry ones in the tree, stays legal.
        let table: Vec<String> = synthetic(16).map(|w| format!("0x{w:08x}_u32")).collect();
        let src = format!(
            "const T: [u32; 16] = [\n    {},\n];\n",
            table.join(",\n    ")
        );
        assert!(check("src/table.rs", src.as_bytes()).is_empty());
    }
}
