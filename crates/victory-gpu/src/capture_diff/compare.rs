use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::{HEADER, Target};

type Manifests = BTreeMap<u64, Vec<(u64, PathBuf)>>;
type Targets = BTreeMap<(usize, Option<usize>), Target>;

fn number<T: FromStr>(s: &str) -> Result<T> {
    ensure!(
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()),
        "expected a decimal integer, got {s:?}"
    );
    s.parse().map_err(|_| anyhow!("integer out of range: {s}"))
}

fn manifests(dir: &Path) -> Result<Manifests> {
    let mut files = Manifests::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "targets") {
            continue;
        }
        let (frame, sub) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("frame-"))
            .and_then(|s| s.split_once("-s"))
            .with_context(|| format!("invalid capture manifest name: {}", path.display()))?;
        let frame =
            number(frame).with_context(|| format!("invalid frame in {}", path.display()))?;
        let sub =
            number(sub).with_context(|| format!("invalid submission in {}", path.display()))?;
        files.entry(frame).or_default().push((sub, path));
    }
    for (frame, submissions) in &mut files {
        submissions.sort_unstable_by_key(|(sub, _)| *sub);
        for pair in submissions.windows(2) {
            ensure!(
                pair[0].0 != pair[1].0,
                "duplicate manifest for frame {frame} submission {} in {}",
                pair[0].0,
                dir.display()
            );
        }
    }
    ensure!(
        !files.is_empty(),
        "no .targets manifests in {}",
        dir.display()
    );
    Ok(files)
}

fn parse_target(line: &str) -> Result<Target> {
    let fields: Vec<_> = line.split('\t').collect();
    let [slot, plane, width, height, depth, format, hash, writer] = fields.as_slice() else {
        bail!("expected eight tab-separated fields");
    };
    Ok(Target {
        slot: number(slot)?,
        plane: if *plane == "-" {
            None
        } else {
            Some(number(plane)?)
        },
        width: number(width)?,
        height: number(height)?,
        depth: number(depth)?,
        format: number(format)?,
        hash: if *hash == "-" {
            None
        } else {
            Some(blake3::Hash::from_hex(hash).context("invalid BLAKE3 digest")?)
        },
        writer: (*writer).to_owned(),
    })
}

fn read_targets(path: &Path) -> Result<Targets> {
    let contents = fs::read_to_string(path)?;
    let mut lines = contents.lines();
    ensure!(lines.next() == Some(HEADER), "expected {HEADER:?} header");
    let mut records = BTreeMap::new();
    for (i, line) in lines.enumerate() {
        let target = parse_target(line).with_context(|| format!("line {}", i + 2))?;
        let key = (target.slot, target.plane);
        ensure!(
            records.insert(key, target).is_none(),
            "duplicate target on line {}",
            i + 2
        );
    }
    Ok(records)
}

fn describe(target: Option<&Target>) -> String {
    let Some(t) = target else {
        return "missing".into();
    };
    let writer = if t.writer.is_empty() {
        "unknown"
    } else {
        &t.writer
    };
    let hash = t
        .hash
        .map_or_else(|| "unavailable".into(), |h| h.to_string());
    format!(
        "{}x{}x{} format {} BLAKE3 {hash} writer {writer}",
        t.width, t.height, t.depth, t.format
    )
}

fn difference(before: Option<&Targets>, after: Option<&Targets>) -> Result<Option<String>> {
    let missing_submission = match (before, after) {
        (None, _) => Some("submission missing before"),
        (_, None) => Some("submission missing after"),
        _ => None,
    };
    let slots: BTreeSet<_> = before
        .into_iter()
        .chain(after)
        .flat_map(|m| m.keys())
        .collect();
    for key @ (slot, plane) in slots {
        let a = before.and_then(|m| m.get(key));
        let b = after.and_then(|m| m.get(key));
        let plane = plane.map_or_else(|| "-".into(), |p| p.to_string());
        if a.into_iter().chain(b).any(|target| target.hash.is_none()) {
            bail!(
                "slot {slot} plane {plane}: target readback unavailable\n  before: {}\n  after: {}",
                describe(a),
                describe(b)
            );
        }
        let reason = match (a, b) {
            (Some(a), Some(b)) => {
                if (a.width, a.height, a.depth, a.format) != (b.width, b.height, b.depth, b.format)
                {
                    "target description changed"
                } else if a.hash != b.hash {
                    "target contents changed"
                } else {
                    continue;
                }
            }
            (None, _) => missing_submission.unwrap_or("target missing before"),
            (_, None) => missing_submission.unwrap_or("target missing after"),
        };
        return Ok(Some(format!(
            "slot {slot} plane {plane}: {reason}\n  before: {}\n  after: {}",
            describe(a),
            describe(b)
        )));
    }
    // Even a submission that produced no targets is present in a capture.
    Ok(missing_submission.map(str::to_owned))
}

/// Compares raw target digests and resource descriptions in frame, submission,
/// slot and plane order. Writer labels explain a difference in the output.
pub fn compare(before: &Path, after: &Path) -> Result<Option<String>> {
    let before = manifests(before)?;
    let after = manifests(after)?;
    let frames: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for frame in frames {
        let a = before.get(frame).map(Vec::as_slice).unwrap_or_default();
        let b = after.get(frame).map(Vec::as_slice).unwrap_or_default();
        let frame_note = if a.is_empty() {
            " (frame missing before)"
        } else if b.is_empty() {
            " (frame missing after)"
        } else {
            ""
        };
        // IDs come from a process-wide counter. Compare the nth capture of this
        // frame to its peer, keeping the original IDs for locating the files.
        for i in 0..a.len().max(b.len()) {
            let read = |(_id, path): &(u64, PathBuf)| {
                read_targets(path).with_context(|| format!("reading {}", path.display()))
            };
            let left = a.get(i).map(read).transpose()?;
            let right = b.get(i).map(read).transpose()?;
            let id = |entry: Option<&(u64, PathBuf)>| {
                entry.map_or_else(|| "missing".into(), |(id, _)| id.to_string())
            };
            let location = || {
                format!(
                    "frame {frame}{frame_note} submission {} (before {}, after {})",
                    i + 1,
                    id(a.get(i)),
                    id(b.get(i))
                )
            };
            match difference(left.as_ref(), right.as_ref()) {
                Ok(Some(diff)) => return Ok(Some(format!("{}: {diff}", location()))),
                Ok(None) => {}
                Err(error) => bail!("{}: {error}", location()),
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture_diff::write_submission;

    fn target(slot: usize, plane: Option<usize>, data: &[u8], writer: &str) -> Target {
        Target {
            slot,
            plane,
            width: 4,
            height: 2,
            depth: 1,
            format: 28,
            hash: Some(blake3::hash(data)),
            writer: writer.to_owned(),
        }
    }

    #[test]
    fn first_changed_target_uses_numeric_order_and_submission_order_within_a_frame() {
        let before = tempfile::tempdir().unwrap();
        let after = tempfile::tempdir().unwrap();
        let (a, b) = (before.path(), after.path());
        // The real IDs drift within a run. Two identical outputs with different
        // writer labels still compare equal.
        for (frame, left, right) in [(2, 9999, 89), (10, 10003, 800)] {
            write_submission(a, frame, left, &[target(0, None, b"same", "old")]).unwrap();
            write_submission(b, frame, right, &[target(0, None, b"same", "other")]).unwrap();
        }
        assert_eq!(compare(a, b).unwrap(), None);
        write_submission(
            a,
            2,
            10000,
            &[
                target(10, None, b"left", "later slot"),
                target(2, Some(10), b"left", "later plane"),
                target(2, Some(2), b"left", "first writer"),
            ],
        )
        .unwrap();
        write_submission(
            b,
            2,
            109,
            &[
                target(2, Some(2), b"right", "second writer"),
                target(2, Some(10), b"right", "later plane"),
                target(10, None, b"right", "later slot"),
            ],
        )
        .unwrap();
        write_submission(a, 10, 10004, &[target(0, None, b"later frame", "")]).unwrap();
        fs::write(a.join("frame-20-s0.targets"), b"unreadable later manifest").unwrap();
        let report = compare(a, b).unwrap().unwrap();
        assert!(report.contains("frame 2 submission 2 (before 10000, after 109): slot 2 plane 2: target contents changed"), "{report}");
        assert!(
            report.contains("first writer") && report.contains("second writer"),
            "{report}"
        );
    }

    #[test]
    fn missing_submissions_and_target_planes_differ_from_empty_outputs() {
        let before = tempfile::tempdir().unwrap();
        let after = tempfile::tempdir().unwrap();
        let (a, b) = (before.path(), after.path());
        write_submission(a, 3, 18, &[]).unwrap();
        write_submission(b, 3, 2, &[]).unwrap();
        assert_eq!(compare(a, b).unwrap(), None);
        write_submission(b, 6, 16, &[]).unwrap();
        assert!(
            compare(a, b)
                .unwrap()
                .unwrap()
                .contains("frame 6 (frame missing before)")
        );
        write_submission(a, 3, 19, &[]).unwrap();
        assert!(
            compare(a, b)
                .unwrap()
                .unwrap()
                .contains("submission missing after")
        );
        write_submission(b, 3, 5, &[target(0, Some(1), b"plane", "CS")]).unwrap();
        let report = compare(a, b).unwrap().unwrap();
        assert!(
            report.contains("slot 0 plane 1: target missing before")
                && report.contains("writer CS"),
            "{report}"
        );
    }

    #[test]
    fn equal_bytes_with_other_dimensions_or_format_change_the_target() {
        let before = tempfile::tempdir().unwrap();
        write_submission(before.path(), 0, 0, &[target(0, None, b"same", "")]).unwrap();
        for (width, height, depth, format) in
            [(8, 2, 1, 28), (4, 4, 1, 28), (4, 2, 2, 28), (4, 2, 1, 29)]
        {
            let after = tempfile::tempdir().unwrap();
            let changed = Target {
                width,
                height,
                depth,
                format,
                ..target(0, None, b"same", "")
            };
            write_submission(after.path(), 0, 1, &[changed]).unwrap();
            let report = compare(before.path(), after.path()).unwrap().unwrap();
            assert!(report.contains("description changed") && report.contains("writer unknown"));
        }
    }

    #[test]
    fn matching_targets_before_an_unavailable_readback_cannot_compare_equal() {
        let before = tempfile::tempdir().unwrap();
        let unreadable = Target {
            hash: None,
            ..target(4, None, b"", "draw")
        };
        let single_sample = target(0, None, b"same", "draw");
        write_submission(before.path(), 4, 10, &[single_sample, unreadable]).unwrap();
        let error = compare(before.path(), before.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("frame 4")
                && error.contains("slot 4 plane -")
                && error.contains("unavailable")
                && error.contains("writer draw"),
            "{error}"
        );
    }

    #[test]
    fn missing_malformed_and_duplicate_manifests_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        fs::write(dir.join("frame-3.png"), b"old capture").unwrap();
        let compare_self = || compare(dir, dir);
        assert!(compare_self().is_err());
        let path = dir.join("frame-3-s1.targets");
        for (invalid, expected) in [
            ("old format\n".into(), "header"),
            (
                format!("{HEADER}\n0\t-\t4\t2\t1\t28\tbad-hash\twriter\n"),
                "invalid BLAKE3 digest",
            ),
            (
                format!("{HEADER}\n0\t-\t4\t2\t1\t28\t-\n"),
                "eight tab-separated fields",
            ),
            (
                format!("{HEADER}\n0\t-\t4\t2\t1\t28\t-\t\n0\t-\t4\t2\t1\t28\t-\t\n"),
                "duplicate target",
            ),
        ] {
            fs::write(&path, invalid).unwrap();
            let error = format!("{:#}", compare_self().unwrap_err());
            assert!(error.contains(expected), "{error}");
        }
        fs::write(&path, format!("{HEADER}\n")).unwrap();
        fs::copy(path, dir.join("frame-3-s01.targets")).unwrap();
        assert!(
            compare_self()
                .unwrap_err()
                .to_string()
                .contains("duplicate manifest")
        );
    }
}
