use std::fs::File;
use std::io::Read;
use std::path::Path;

const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Accept native UTF-8 logs and BOM-marked PowerShell UTF-16 output.
/// Bound memory use and reject malformed input instead of silently replacing it.
pub(super) fn read(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("log exceeds 64 MiB".into());
    }
    decode(&bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<String, String> {
    if let Some((payload, little)) = bytes
        .strip_prefix(&[0xff, 0xfe])
        .map(|b| (b, true))
        .or_else(|| bytes.strip_prefix(&[0xfe, 0xff]).map(|b| (b, false)))
    {
        let (words, remainder) = payload.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err("truncated UTF-16 code unit".into());
        }
        let words: Vec<u16> = words
            .iter()
            .map(|&b| {
                if little {
                    u16::from_le_bytes(b)
                } else {
                    u16::from_be_bytes(b)
                }
            })
            .collect();
        String::from_utf16(&words).map_err(|_| "invalid UTF-16".into())
    } else {
        let payload = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        String::from_utf8(payload.to_vec()).map_err(|_| "invalid UTF-8".into())
    }
}
