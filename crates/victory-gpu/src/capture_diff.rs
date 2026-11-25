//! Compare the raw render-target contents recorded by the diagnostics build.

mod compare;
pub use compare::compare;

const HEADER: &str = "VIctory targets v1";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub slot: usize,
    pub plane: Option<usize>,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub format: u32,
    pub hash: Option<blake3::Hash>,
    pub writer: String,
}

/// D3D12 can pad each row, including the last row of a depth slice.
#[cfg(any(all(windows, feature = "diagnostics"), test))]
pub(crate) fn hash_rows(
    rows: &[u8],
    row_bytes: usize,
    row_pitch: usize,
    row_count: usize,
    depth: usize,
) -> Option<blake3::Hash> {
    if row_bytes == 0 || row_pitch < row_bytes || row_count == 0 || depth == 0 {
        return None;
    }
    let count = row_count.checked_mul(depth)?;
    let len = (count - 1).checked_mul(row_pitch)?.checked_add(row_bytes)?;
    let mut hash = blake3::Hasher::new();
    for row in rows.get(..len)?.chunks(row_pitch) {
        hash.update(&row[..row_bytes]);
    }
    Some(hash.finalize())
}

/// Publish a complete submission. Readers ignore staging files.
#[cfg(any(all(windows, feature = "diagnostics"), test))]
pub(crate) fn write_submission(
    dir: &std::path::Path,
    frame: u64,
    submission: u64,
    targets: &[Target],
) -> std::io::Result<()> {
    use std::fmt::Write;
    use std::{fs, io};

    let mut text = String::from(HEADER);
    text.push('\n');
    for t in targets {
        if t.writer.contains(['\r', '\n', '\t']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid capture writer name",
            ));
        }
        let plane = t.plane.map_or_else(|| "-".to_owned(), |p| p.to_string());
        let hash = t.hash.map_or_else(|| "-".to_owned(), |h| h.to_string());
        writeln!(
            text,
            "{}\t{plane}\t{}\t{}\t{}\t{}\t{hash}\t{}",
            t.slot, t.width, t.height, t.depth, t.format, t.writer
        )
        .expect("writing to a String");
    }
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("frame-{frame}-s{submission:04}.targets"));
    let staging = path.with_extension(format!("targets.{}.tmp", std::process::id()));
    fs::write(&staging, text)?;
    fs::rename(staging, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_all_logical_rows_and_depth_slices_without_padding() {
        let pixels = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut padded = [1, 2, 10, 10, 3, 4, 20, 20, 5, 6, 30, 30, 7, 8];
        let expected = Some(blake3::hash(&pixels));
        assert_eq!(hash_rows(&pixels, 2, 2, 2, 2), expected);
        assert_eq!(hash_rows(&padded, 2, 4, 2, 2), expected);
        padded[13] += 1;
        assert_ne!(hash_rows(&padded, 2, 4, 2, 2), expected);
    }

    #[test]
    fn rejects_short_or_overflowing_footprints() {
        assert_eq!(hash_rows(&[0; 5], 2, 4, 2, 1), None);
        assert_eq!(hash_rows(&[0; 4], 3, 2, 1, 1), None);
        assert_eq!(hash_rows(&[], 1, 2, usize::MAX, 2), None);
        assert_eq!(hash_rows(&[], 1, usize::MAX, 2, 1), None);
        assert_eq!(hash_rows(&[], 0, 0, 0, 0), None);
    }
}
