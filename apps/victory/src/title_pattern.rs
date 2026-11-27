use victory_loader::LoadedImage;

/// The rva of the one match of `pattern` in the image's executable sections.
/// None, said why, for no match or several.
pub(crate) fn find_once(image: &LoadedImage, what: &str, pattern: &str) -> Option<usize> {
    const DISCARDABLE: u32 = 0x0200_0000;
    const EXECUTE: u32 = 0x2000_0000;
    const READ: u32 = 0x4000_0000;
    let pattern = parse_pattern(pattern);
    let mut found = Vec::new();
    for s in image
        .info()
        .sections
        .iter()
        .filter(|s| s.characteristics & EXECUTE != 0)
    {
        let start = s.rva as usize;
        let len = s.virtual_size as usize;
        if len == 0 {
            continue;
        }
        if s.characteristics & (DISCARDABLE | READ) != READ
            || start
                .checked_add(len)
                .is_none_or(|end| end > image.info().size_of_image as usize)
        {
            eprintln!(
                "VIctory: {what} not patched: cannot scan executable section {}",
                s.name
            );
            return None;
        }
        // SAFETY: Windows mapped this readable, non-discardable section
        // inside the image and no title or diagnostic thread is running.
        let code = unsafe { std::slice::from_raw_parts((image.base() + start) as *const u8, len) };
        found.extend(matches(code, &pattern).into_iter().map(|at| start + at));
    }
    match found[..] {
        [rva] => Some(rva),
        [] => {
            eprintln!("VIctory: {what} not patched: its code is not in this build of the title");
            None
        }
        _ => {
            eprintln!(
                "VIctory: {what} not patched: its pattern matches {} places in this build",
                found.len()
            );
            None
        }
    }
}

/// A pattern of hex bytes and `??` (any byte), space-separated.
pub(crate) fn parse_pattern(pattern: &str) -> Vec<Option<u8>> {
    pattern
        .split_whitespace()
        .map(|b| (b != "??").then(|| u8::from_str_radix(b, 16).expect("a hex byte or ??")))
        .collect()
}

/// The offsets in `code` where `pattern` matches.
pub(crate) fn matches(code: &[u8], pattern: &[Option<u8>]) -> Vec<usize> {
    // Look for its longest fixed run, then check the rest around each hit.
    let (anchor_at, anchor) = longest_fixed_run(pattern);
    memchr::memmem::find_iter(code, &anchor)
        .filter_map(|hit| hit.checked_sub(anchor_at))
        .filter(|&start| {
            code.get(start..start + pattern.len())
                .is_some_and(|window| {
                    window
                        .iter()
                        .zip(pattern)
                        .all(|(b, p)| p.is_none_or(|p| p == *b))
                })
        })
        .collect()
}

/// Where the longest run of fixed bytes in `pattern` starts, and its bytes.
pub(crate) fn longest_fixed_run(pattern: &[Option<u8>]) -> (usize, Vec<u8>) {
    let (mut best, mut start) = ((0, 0), 0);
    for (i, p) in pattern.iter().enumerate() {
        if p.is_none() {
            start = i + 1;
        } else if i + 1 - start > best.1 {
            best = (start, i + 1 - start);
        }
    }
    let run = pattern[best.0..best.0 + best.1].iter().flatten().copied();
    (best.0, run.collect())
}
