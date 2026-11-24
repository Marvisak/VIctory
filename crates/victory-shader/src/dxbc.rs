//! The DXBC container: a header, then a table of FourCC-tagged parts.
//!
//! ```text
//! 0x00  "DXBC"
//! 0x04  digest[16]
//! 0x14  version major u16, minor u16
//! 0x18  total size u32
//! 0x1C  part count u32
//! 0x20  part offsets u32[count]
//! part: fourcc[4], size u32, data[size]
//! ```

use crate::ShaderError;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    pub const DXIL: FourCC = FourCC(*b"DXIL");
    pub const STAT: FourCC = FourCC(*b"STAT");
    pub const RTS0: FourCC = FourCC(*b"RTS0");
    pub const PSV0: FourCC = FourCC(*b"PSV0");
    pub const ISG1: FourCC = FourCC(*b"ISG1");
    pub const OSG1: FourCC = FourCC(*b"OSG1");
    /// Per-primitive outputs of a mesh shader.
    pub const PSG1: FourCC = FourCC(*b"PSG1");
    pub const SFI0: FourCC = FourCC(*b"SFI0");
    pub const HASH: FourCC = FourCC(*b"HASH");
    pub const ILDB: FourCC = FourCC(*b"ILDB");
    pub const ILDN: FourCC = FourCC(*b"ILDN");
    /// Runtime data for libraries and work graphs.
    pub const RDAT: FourCC = FourCC(*b"RDAT");
    /// The compiler's version, commit and custom string.
    pub const VERS: FourCC = FourCC(*b"VERS");
    /// Embedded source and compile arguments.
    pub const SRCI: FourCC = FourCC(*b"SRCI");
    /// The PDB's own description of the shader.
    pub const PDBI: FourCC = FourCC(*b"PDBI");

    /// The parts DXC knows. Its validator refuses a container that lists one
    /// of them twice ("More than one ... part"); other FourCCs are a title's
    /// own and may repeat.
    const KNOWN: [FourCC; 15] = [
        Self::DXIL,
        Self::STAT,
        Self::RTS0,
        Self::PSV0,
        Self::ISG1,
        Self::OSG1,
        Self::PSG1,
        Self::SFI0,
        Self::HASH,
        Self::ILDB,
        Self::ILDN,
        Self::RDAT,
        Self::VERS,
        Self::SRCI,
        Self::PDBI,
    ];
}

impl std::fmt::Debug for FourCC {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl std::fmt::Display for FourCC {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for &b in &self.0 {
            let c = if b.is_ascii_graphic() { b as char } else { '.' };
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Part<'a> {
    pub fourcc: FourCC,
    /// Offset of the part header within the container.
    pub offset: usize,
    pub data: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct Container<'a> {
    pub digest: [u8; 16],
    pub version: (u16, u16),
    pub parts: Vec<Part<'a>>,
}

const HEADER_SIZE: usize = 0x20;
const MAX_PARTS: usize = 256;
/// DXC's `DxilContainerMaxSize`: `IsValidDxilContainer` refuses a larger
/// container, and offsets past it no longer fit a signed 32-bit integer.
const MAX_SIZE: usize = 0x8000_0000;

impl<'a> Container<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, ShaderError> {
        if data.len() < HEADER_SIZE || &data[0..4] != b"DXBC" {
            return Err(ShaderError::NotDxbc);
        }
        let digest = data[4..20].try_into().expect("16 bytes");
        let version = (u16_at(data, 0x14)?, u16_at(data, 0x16)?);
        // DXC writes 1.0 and its `IsValidDxilContainer` refuses any other
        // major: a different one is a layout this parser does not know.
        if version.0 != 1 {
            return Err(ShaderError::Malformed("container major version is not 1"));
        }
        let total = u32_at(data, 0x18)? as usize;
        // Checked before the data length, so a forged size is named as such
        // even when the buffer is short.
        if total > MAX_SIZE {
            return Err(ShaderError::Malformed("container larger than DXC allows"));
        }
        if total > data.len() || total < HEADER_SIZE {
            return Err(ShaderError::Malformed("container size disagrees with data"));
        }
        let data = &data[..total];
        let count = u32_at(data, 0x1C)? as usize;
        if count > MAX_PARTS {
            return Err(ShaderError::Malformed("implausible part count"));
        }
        // Parts follow the offset table; one inside it would read the
        // header or the offsets as a part.
        let table_end = HEADER_SIZE + 4 * count;
        let mut parts = Vec::with_capacity(count);
        for i in 0..count {
            let offset = u32_at(data, HEADER_SIZE + 4 * i)? as usize;
            if offset < table_end {
                return Err(ShaderError::Malformed("part overlaps the container header"));
            }
            let part = data
                .get(offset..)
                .ok_or(ShaderError::Malformed("part header out of bounds"))?;
            let fourcc = FourCC(
                *part
                    .first_chunk()
                    .ok_or(ShaderError::Malformed("part header out of bounds"))?,
            );
            let size = u32_at(part, 4)? as usize;
            let body = part[8..]
                .get(..size)
                .ok_or(ShaderError::Malformed("part data out of bounds"))?;
            // `part` returns the first of a FourCC, so a second `ISG1` or
            // `DXIL` was silently ignored while the host, the validator or a
            // tool reading the container could take the other one.
            if FourCC::KNOWN.contains(&fourcc) && parts.iter().any(|p: &Part| p.fourcc == fourcc) {
                return Err(ShaderError::Malformed("a part listed twice"));
            }
            // A part whose bytes another part also claims reads that part's
            // header or data as its own; DXC lays parts end to end.
            let end = offset + 8 + size;
            if parts
                .iter()
                .any(|p: &Part| offset < p.offset + 8 + p.data.len() && p.offset < end)
            {
                return Err(ShaderError::Malformed("two parts overlap"));
            }
            parts.push(Part {
                fourcc,
                offset,
                data: body,
            });
        }
        Ok(Container {
            digest,
            version,
            parts,
        })
    }

    pub fn part(&self, fourcc: FourCC) -> Option<&Part<'a>> {
        self.parts.iter().find(|p| p.fourcc == fourcc)
    }
}

pub(crate) fn u16_at(data: &[u8], at: usize) -> Result<u16, ShaderError> {
    data.get(at..)
        .and_then(|tail| tail.first_chunk())
        .map(|b| u16::from_le_bytes(*b))
        .ok_or(ShaderError::Malformed("truncated"))
}

pub(crate) fn u32_at(data: &[u8], at: usize) -> Result<u32, ShaderError> {
    data.get(at..)
        .and_then(|tail| tail.first_chunk())
        .map(|b| u32::from_le_bytes(*b))
        .ok_or(ShaderError::Malformed("truncated"))
}

pub(crate) fn u64_at(data: &[u8], at: usize) -> Result<u64, ShaderError> {
    data.get(at..)
        .and_then(|tail| tail.first_chunk())
        .map(|b| u64::from_le_bytes(*b))
        .ok_or(ShaderError::Malformed("truncated"))
}

/// Builds a container from parts (test helper).
#[cfg(test)]
pub fn build(parts: &[(FourCC, &[u8])]) -> Vec<u8> {
    let table = HEADER_SIZE + 4 * parts.len();
    let total = table + parts.iter().map(|(_, d)| 8 + d.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"DXBC");
    out.extend_from_slice(&[0; 16]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    let mut offset = table;
    for (_, d) in parts {
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += 8 + d.len();
    }
    for (fourcc, d) in parts {
        out.extend_from_slice(&fourcc.0);
        out.extend_from_slice(&(d.len() as u32).to_le_bytes());
        out.extend_from_slice(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_parts() {
        let bytes = build(&[(FourCC::SFI0, &[1, 2, 3, 4]), (FourCC::HASH, &[9; 20])]);
        let c = Container::parse(&bytes).unwrap();
        assert_eq!(c.parts.len(), 2);
        assert_eq!(c.part(FourCC::SFI0).unwrap().data, &[1, 2, 3, 4]);
        assert_eq!(c.part(FourCC::HASH).unwrap().data.len(), 20);
        assert!(c.part(FourCC::DXIL).is_none());
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Container::parse(b"nope").unwrap_err(), ShaderError::NotDxbc);
        let mut bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        let last = bytes.len() - 1;
        bytes[0x20] = 0xFF; // corrupt the first part offset
        assert!(Container::parse(&bytes).is_err());
        bytes.truncate(last);
        assert!(Container::parse(&bytes).is_err());
    }

    fn malformed(bytes: &[u8]) -> &'static str {
        match Container::parse(bytes).unwrap_err() {
            ShaderError::Malformed(why) => why,
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn rejects_sizes_that_disagree_with_the_data() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        let mut larger = bytes.clone();
        larger[0x18..0x1C].copy_from_slice(&(bytes.len() as u32 + 1).to_le_bytes());
        assert_eq!(malformed(&larger), "container size disagrees with data");
        let mut smaller = bytes.clone();
        smaller[0x18..0x1C].copy_from_slice(&(HEADER_SIZE as u32 - 1).to_le_bytes());
        assert_eq!(malformed(&smaller), "container size disagrees with data");
        // Bytes past the stated size are not part of the container.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0xAA; 16]);
        assert_eq!(Container::parse(&padded).unwrap().parts.len(), 1);
    }

    #[test]
    fn rejects_a_container_past_dxcs_maximum_size() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        for size in [MAX_SIZE as u32 + 1, u32::MAX] {
            let mut bad = bytes.clone();
            bad[0x18..0x1C].copy_from_slice(&size.to_le_bytes());
            assert_eq!(
                malformed(&bad),
                "container larger than DXC allows",
                "size {size:#x}"
            );
        }
        // Exactly the maximum is a size, not a forgery; here it only
        // disagrees with the (short) data.
        let mut max = bytes;
        max[0x18..0x1C].copy_from_slice(&(MAX_SIZE as u32).to_le_bytes());
        assert_eq!(malformed(&max), "container size disagrees with data");
    }

    #[test]
    fn rejects_a_container_major_version_other_than_1() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        for major in [0u16, 2, 0xFFFF] {
            let mut bad = bytes.clone();
            bad[0x14..0x16].copy_from_slice(&major.to_le_bytes());
            assert_eq!(
                malformed(&bad),
                "container major version is not 1",
                "major {major}"
            );
        }
        // The minor version is DXC's to bump; any is kept.
        let mut minor = bytes;
        minor[0x16..0x18].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(Container::parse(&minor).unwrap().version, (1, 7));
    }

    #[test]
    fn rejects_implausible_part_counts() {
        let mut bytes = build(&[]);
        bytes[0x1C..0x20].copy_from_slice(&(MAX_PARTS as u32 + 1).to_le_bytes());
        assert_eq!(malformed(&bytes), "implausible part count");
        // A count whose offset table runs past the container.
        bytes[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(malformed(&bytes), "truncated");
    }

    #[test]
    fn rejects_parts_out_of_bounds() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        let mut header = bytes.clone();
        header[0x20..0x24].copy_from_slice(&(bytes.len() as u32 - 2).to_le_bytes());
        assert_eq!(malformed(&header), "part header out of bounds");
        header[0x20..0x24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(malformed(&header), "part header out of bounds");
        for length in [9u32, u32::MAX] {
            let mut size = bytes.clone();
            size[0x28..0x2C].copy_from_slice(&length.to_le_bytes());
            assert_eq!(malformed(&size), "part data out of bounds");
        }
    }

    #[test]
    fn integer_reads_check_the_offset_and_width() {
        let bytes = [0x42; 8];
        assert_eq!(u16_at(&bytes, 6), Ok(0x4242));
        assert_eq!(u32_at(&bytes, 4), Ok(0x4242_4242));
        assert_eq!(u64_at(&bytes, 0), Ok(0x4242_4242_4242_4242));
        let truncated = ShaderError::Malformed("truncated");
        for offset in [7, usize::MAX] {
            assert_eq!(u16_at(&bytes, offset).unwrap_err(), truncated);
            assert_eq!(u32_at(&bytes, offset).unwrap_err(), truncated);
            assert_eq!(u64_at(&bytes, offset).unwrap_err(), truncated);
        }
    }

    #[test]
    fn rejects_parts_inside_the_header() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8]), (FourCC::HASH, &[0; 4])]);
        // Offset 0 would read "DXBC" as a part; the offset table's own
        // second entry is no part either.
        for offset in [0u32, 0x10, HEADER_SIZE as u32 + 4] {
            let mut bad = bytes.clone();
            bad[0x20..0x24].copy_from_slice(&offset.to_le_bytes());
            assert_eq!(
                malformed(&bad),
                "part overlaps the container header",
                "offset {offset:#x}"
            );
        }
        // The first byte after the table is where parts start.
        let c = Container::parse(&bytes).unwrap();
        assert_eq!(c.parts[0].offset, HEADER_SIZE + 8);
    }

    #[test]
    fn overlapping_parts_are_refused() {
        // A title's own FourCC first, so the same offset twice is not
        // refused as a known part listed twice.
        let own = FourCC(*b"PRIV");
        let bytes = build(&[(own, &[0; 8]), (FourCC::HASH, &[0; 4])]);
        let first = HEADER_SIZE as u32 + 8;
        let second = first + 16;
        // The second part starting inside the first's header, inside its
        // data, or on its very offset.
        for offset in [first, first + 4, first + 8] {
            let mut bad = bytes.clone();
            bad[0x24..0x28].copy_from_slice(&offset.to_le_bytes());
            assert_eq!(malformed(&bad), "two parts overlap", "offset {offset:#x}");
        }
        // The first part's size growing over the second's header.
        let mut grown = bytes.clone();
        grown[first as usize + 4..first as usize + 8].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(malformed(&grown), "two parts overlap");
        // Listed in reverse order, end to end, the parts still parse.
        let mut swapped = bytes.clone();
        swapped[0x20..0x24].copy_from_slice(&second.to_le_bytes());
        swapped[0x24..0x28].copy_from_slice(&first.to_le_bytes());
        let c = Container::parse(&swapped).unwrap();
        assert_eq!(c.parts[0].fourcc, FourCC::HASH);
        assert_eq!(c.parts[1].fourcc, own);
        // A gap between parts is not an overlap.
        let mut gap = bytes.clone();
        gap[first as usize + 4..first as usize + 8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(Container::parse(&gap).unwrap().parts[0].data.len(), 4);
    }

    #[test]
    fn a_known_part_listed_twice_is_refused() {
        for fourcc in FourCC::KNOWN {
            let bytes = build(&[
                (fourcc, &[1; 4]),
                (FourCC::SFI0, &[0; 8]),
                (fourcc, &[2; 4]),
            ]);
            assert_eq!(malformed(&bytes), "a part listed twice", "{fourcc}");
        }
        // Each known part once, and a title's own part twice, still parse.
        let once: Vec<_> = FourCC::KNOWN.iter().map(|&f| (f, &[0u8; 4][..])).collect();
        assert_eq!(
            Container::parse(&build(&once)).unwrap().parts.len(),
            FourCC::KNOWN.len()
        );
        let own = FourCC(*b"PRIV");
        let twice = build(&[(own, &[1; 4]), (FourCC::DXIL, &[0; 4]), (own, &[2; 4])]);
        let c = Container::parse(&twice).unwrap();
        assert_eq!(c.parts.len(), 3);
        assert_eq!(c.part(own).unwrap().data, &[1; 4]);
    }

    #[test]
    fn empty_parts_and_a_part_ending_at_the_container_end_parse() {
        let bytes = build(&[(FourCC::SFI0, &[]), (FourCC::HASH, &[7; 4])]);
        let c = Container::parse(&bytes).unwrap();
        assert!(c.part(FourCC::SFI0).unwrap().data.is_empty());
        // The empty part's header ends where the next one starts.
        assert_eq!(c.parts[1].offset, c.parts[0].offset + 8);
        let last = c.part(FourCC::HASH).unwrap();
        assert_eq!(last.offset + 8 + last.data.len(), bytes.len());
        // One byte less and the last part's data runs past the container.
        let mut short = bytes.clone();
        short[0x18..0x1C].copy_from_slice(&(bytes.len() as u32 - 1).to_le_bytes());
        assert_eq!(malformed(&short), "part data out of bounds");
    }

    #[test]
    fn a_part_header_cut_after_its_fourcc_is_truncated() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        // The FourCC fits; the size word after it does not.
        for back in [4u32, 5, 7] {
            let mut bad = bytes.clone();
            bad[0x20..0x24].copy_from_slice(&(bytes.len() as u32 - back).to_le_bytes());
            assert_eq!(malformed(&bad), "truncated", "{back} bytes from the end");
        }
        // Exactly a header, with a zero size, at the very end is a part.
        let mut header = bytes.clone();
        header[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes());
        header[0x20..0x24].copy_from_slice(&(bytes.len() as u32 - 8).to_le_bytes());
        header[bytes.len() - 8..bytes.len() - 4].copy_from_slice(b"PRIV");
        header[bytes.len() - 4..].copy_from_slice(&0u32.to_le_bytes());
        let c = Container::parse(&header).unwrap();
        assert_eq!(c.parts[0].fourcc, FourCC(*b"PRIV"));
        assert!(c.parts[0].data.is_empty());
    }

    #[test]
    fn exactly_the_maximum_part_count_parses() {
        let own = FourCC(*b"PRIV");
        let parts = vec![(own, &[][..]); MAX_PARTS];
        let bytes = build(&parts);
        let c = Container::parse(&bytes).unwrap();
        assert_eq!(c.parts.len(), MAX_PARTS);
        assert_eq!(c.parts[0].offset, HEADER_SIZE + 4 * MAX_PARTS);
        // An empty container lists no parts at all.
        assert!(Container::parse(&build(&[])).unwrap().parts.is_empty());
    }

    #[test]
    fn parts_and_offsets_past_the_stated_size_are_out_of_bounds() {
        let bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        let total = bytes.len() as u32;
        // A part starting exactly at the container end has no header.
        let mut at_end = bytes.clone();
        at_end[0x20..0x24].copy_from_slice(&total.to_le_bytes());
        assert_eq!(malformed(&at_end), "part header out of bounds");
        // A whole, valid part in bytes after the stated size is still
        // outside the container, though the buffer holds it.
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(b"PRIV");
        trailing.extend_from_slice(&0u32.to_le_bytes());
        trailing[0x20..0x24].copy_from_slice(&total.to_le_bytes());
        assert_eq!(malformed(&trailing), "part header out of bounds");
        // An offset table the stated size cuts short is truncated, even
        // when the buffer goes on past it.
        let mut table = build(&[]);
        table[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes());
        table.extend_from_slice(&(HEADER_SIZE as u32 + 4).to_le_bytes());
        assert_eq!(malformed(&table), "truncated");
        // With the size covering the table, its one entry points at the
        // container end: no header there either.
        table[0x18..0x1C].copy_from_slice(&(HEADER_SIZE as u32 + 4).to_le_bytes());
        assert_eq!(malformed(&table), "part header out of bounds");
    }

    #[test]
    fn the_magic_and_header_length_come_first() {
        // A header one byte short is no container, whatever follows "DXBC".
        let bytes = build(&[]);
        assert_eq!(
            Container::parse(&bytes[..HEADER_SIZE - 1]).unwrap_err(),
            ShaderError::NotDxbc
        );
        // The magic is case-sensitive and checked before the version.
        let mut lower = bytes.clone();
        lower[0..4].copy_from_slice(b"dxbc");
        lower[0x14..0x16].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(Container::parse(&lower).unwrap_err(), ShaderError::NotDxbc);
        // A bare header with no parts is a container, and its digest is
        // carried through untouched.
        let mut digest = bytes.clone();
        digest[4..20].copy_from_slice(&core::array::from_fn::<u8, 16, _>(|i| i as u8 * 17));
        let c = Container::parse(&digest).unwrap();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(c.digest, core::array::from_fn(|i| i as u8 * 17));
        assert_eq!(c.version, (1, 0));
        assert!(c.parts.is_empty());
    }

    #[test]
    fn empty_parts_still_claim_their_header() {
        let own = FourCC(*b"PRIV");
        let bytes = build(&[(own, &[]), (own, &[])]);
        let first = HEADER_SIZE as u32 + 8;
        // Two empty parts end to end parse.
        let c = Container::parse(&bytes).unwrap();
        assert_eq!(c.parts[1].offset, c.parts[0].offset + 8);
        // Moved four bytes back, the second header starts inside the first:
        // it reads the first's zero size as its FourCC, and the old header's
        // FourCC, zeroed here, as its size.
        let mut half = bytes.clone();
        half[0x24..0x28].copy_from_slice(&(first + 4).to_le_bytes());
        half[first as usize + 8..first as usize + 12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(malformed(&half), "two parts overlap");
        // An empty part inside another part's data is an overlap too.
        let bytes = build(&[(own, &[0; 16]), (own, &[])]);
        let mut inside = bytes.clone();
        inside[0x24..0x28].copy_from_slice(&(first + 8).to_le_bytes());
        inside[first as usize + 8..first as usize + 12].copy_from_slice(b"PRIV");
        assert_eq!(malformed(&inside), "two parts overlap");
        // Its header ending on the other part's last byte still overlaps;
        // starting just past it does not.
        let mut tail = bytes.clone();
        tail[0x24..0x28].copy_from_slice(&(first + 8 + 16 - 8).to_le_bytes());
        assert_eq!(malformed(&tail), "two parts overlap");
        assert_eq!(Container::parse(&bytes).unwrap().parts.len(), 2);
    }

    #[test]
    fn known_fourccs_match_case_and_every_byte() {
        // "dxil" is a title's own part, not DXC's, so it may repeat and is
        // never taken for the program.
        let lower = FourCC(*b"dxil");
        let bytes = build(&[(lower, &[1; 4]), (lower, &[2; 4])]);
        let c = Container::parse(&bytes).unwrap();
        assert!(c.part(FourCC::DXIL).is_none());
        assert_eq!(c.part(lower).unwrap().data, &[1; 4]);
        // A FourCC one byte off a known one, or padded with a NUL, repeats
        // freely beside the real part.
        for near in [*b"DXIM", *b"DXI\0", *b"\0XIL"] {
            let near = FourCC(near);
            let bytes = build(&[(FourCC::DXIL, &[3; 4]), (near, &[4; 4]), (near, &[5; 4])]);
            let c = Container::parse(&bytes).unwrap();
            assert_eq!(c.part(FourCC::DXIL).unwrap().data, &[3; 4], "{near}");
            assert_eq!(c.parts.len(), 3);
        }
    }

    #[test]
    fn dxcs_newer_parts_may_not_repeat_either() {
        // DXC also writes runtime data, its version, the embedded source and
        // PDB info, and its validator refuses a second of each like any other.
        for fourcc in [*b"RDAT", *b"VERS", *b"SRCI", *b"PDBI"] {
            let fourcc = FourCC(fourcc);
            let bytes = build(&[
                (FourCC::DXIL, &[0; 4]),
                (fourcc, &[1; 4]),
                (fourcc, &[2; 4]),
            ]);
            assert_eq!(malformed(&bytes), "a part listed twice", "{fourcc}");
            // Once, beside the program, it is simply kept.
            let once = build(&[(FourCC::DXIL, &[0; 4]), (fourcc, &[1; 4])]);
            assert_eq!(
                Container::parse(&once).unwrap().part(fourcc).unwrap().data,
                &[1; 4]
            );
        }
        // DXBC-era signature parts are not DXC's and still repeat freely.
        for fourcc in [*b"ISGN", *b"OSGN", *b"RDEF"] {
            let fourcc = FourCC(fourcc);
            let bytes = build(&[(fourcc, &[1; 4]), (fourcc, &[2; 4])]);
            assert_eq!(Container::parse(&bytes).unwrap().parts.len(), 2, "{fourcc}");
        }
    }

    #[test]
    fn the_first_offset_failure_wins_in_table_order() {
        // An overlap and a bad header in one table: the earlier entry is
        // reported, so a fix to it surfaces the next.
        let own = FourCC(*b"PRIV");
        let bytes = build(&[(own, &[0; 4]), (own, &[0; 4]), (own, &[0; 4])]);
        let first = HEADER_SIZE as u32 + 12;
        let mut bad = bytes.clone();
        bad[0x24..0x28].copy_from_slice(&first.to_le_bytes());
        bad[0x28..0x2C].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(malformed(&bad), "two parts overlap");
        bad[0x24..0x28].copy_from_slice(&(first + 12).to_le_bytes());
        assert_eq!(malformed(&bad), "part overlaps the container header");
    }

    #[test]
    fn overlap_is_checked_against_every_earlier_part() {
        // The third part pointed back into the first's data: the second
        // part, between them in the table, does not hide the overlap.
        let own = FourCC(*b"PRIV");
        let bytes = build(&[(own, &[0; 16]), (own, &[0; 4]), (own, &[0; 4])]);
        let first = HEADER_SIZE as u32 + 12;
        // On its header, at the start of its data, and with the third's
        // header ending exactly where the second part begins.
        for offset in [first, first + 8, first + 8 + 16 - 8] {
            let mut bad = bytes.clone();
            bad[0x28..0x2C].copy_from_slice(&offset.to_le_bytes());
            // A zero size keeps the third part's range inside the first.
            let at = offset as usize;
            bad[at + 4..at + 8].copy_from_slice(&0u32.to_le_bytes());
            assert_eq!(malformed(&bad), "two parts overlap", "offset {offset:#x}");
        }
        assert_eq!(Container::parse(&bytes).unwrap().parts.len(), 3);
    }

    #[test]
    fn a_known_part_listed_twice_at_one_offset_is_a_repeat() {
        // Two table entries naming the same DXIL part read the same bytes
        // twice; the repeat is named before the overlap it also is.
        let bytes = build(&[(FourCC::DXIL, &[0; 4]), (FourCC::SFI0, &[0; 4])]);
        let mut bad = bytes.clone();
        bad[0x24..0x28].copy_from_slice(&(HEADER_SIZE as u32 + 8).to_le_bytes());
        assert_eq!(malformed(&bad), "a part listed twice");
        // A title's own part listed twice at one offset is no repeat, but
        // still an overlap.
        let own = FourCC(*b"PRIV");
        let mut own_bad = build(&[(own, &[0; 4]), (own, &[0; 4])]);
        own_bad[0x24..0x28].copy_from_slice(&(HEADER_SIZE as u32 + 8).to_le_bytes());
        assert_eq!(malformed(&own_bad), "two parts overlap");
    }

    #[test]
    fn fourcc_display_masks_unprintable_bytes() {
        assert_eq!(FourCC::DXIL.to_string(), "DXIL");
        assert_eq!(FourCC(*b"A\0 \xFF").to_string(), "A...");
        assert_eq!(format!("{:?}", FourCC(*b"\nPS\x7F")), ".PS.");
    }

    #[test]
    fn the_major_version_is_checked_before_the_size() {
        // A wrong major names the layout as unknown even when the size and
        // part count are forged too: nothing past the version is trusted.
        let mut bytes = build(&[(FourCC::SFI0, &[0; 8])]);
        bytes[0x14..0x16].copy_from_slice(&2u16.to_le_bytes());
        bytes[0x18..0x1C].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[0x1C..0x20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(malformed(&bytes), "container major version is not 1");
        // With the major fixed, the size is named before the part count.
        bytes[0x14..0x16].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(malformed(&bytes), "container larger than DXC allows");
        let len = bytes.len() as u32;
        bytes[0x18..0x1C].copy_from_slice(&len.to_le_bytes());
        assert_eq!(malformed(&bytes), "implausible part count");
    }
    #[test]
    fn a_part_out_of_bounds_is_named_before_a_repeat() {
        // The second DXIL's data runs past the container: its bounds are
        // checked before its FourCC is compared with the earlier parts.
        let mut bytes = build(&[(FourCC::DXIL, &[0; 4]), (FourCC::DXIL, &[0; 4])]);
        let second = bytes.len() - 12;
        bytes[second + 4..second + 8].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(malformed(&bytes), "part data out of bounds");
        // An empty first copy is still a copy.
        let empty = build(&[(FourCC::ISG1, &[]), (FourCC::ISG1, &[0; 4])]);
        assert_eq!(malformed(&empty), "a part listed twice");
    }

    #[test]
    fn part_data_borrows_the_input_bytes() {
        // Each part's data is a view of the buffer just past its header, at
        // the offset the table gives, not a copy.
        let bytes = build(&[(FourCC::SFI0, &[1, 2, 3]), (FourCC::HASH, &[4; 5])]);
        let c = Container::parse(&bytes).unwrap();
        for p in &c.parts {
            assert_eq!(&bytes[p.offset..p.offset + 4], &p.fourcc.0);
            assert!(std::ptr::eq(
                p.data.as_ptr(),
                bytes[p.offset + 8..].as_ptr()
            ));
        }
        // An odd-sized part leaves the next one unaligned; DXC pads, but the
        // table's offsets are taken as written.
        assert_eq!(c.parts[1].offset % 4, 3);
        assert_eq!(c.part(FourCC::HASH).unwrap().data, &[4; 5]);
    }

    #[test]
    fn a_later_part_reaching_up_into_an_earlier_one_overlaps() {
        // The table lists the higher part first, so the second entry starts
        // below the first and only its end can reach it.
        let own = FourCC(*b"PRIV");
        let bytes = build(&[(own, &[0; 16]), (own, &[0; 4])]);
        let low = HEADER_SIZE + 8;
        let high = low + 8 + 16;
        let mut swapped = bytes.clone();
        swapped[0x20..0x24].copy_from_slice(&(high as u32).to_le_bytes());
        swapped[0x24..0x28].copy_from_slice(&(low as u32).to_le_bytes());
        // Ending exactly where the higher part's header starts is adjacent.
        let c = Container::parse(&swapped).unwrap();
        assert_eq!((c.parts[0].offset, c.parts[1].offset), (high, low));
        // One byte into the higher header, or over all of it and its data
        // to the container end, is an overlap.
        for size in [17u32, 16 + 12] {
            let mut bad = swapped.clone();
            bad[low + 4..low + 8].copy_from_slice(&size.to_le_bytes());
            assert_eq!(malformed(&bad), "two parts overlap", "size {size}");
        }
        // The enclosing size alone is in bounds: listed first, the enclosed
        // part is refused against it, and with the table cut to that one
        // entry it parses, the other header now just bytes of its data.
        let mut first = bytes.clone();
        first[low + 4..low + 8].copy_from_slice(&28u32.to_le_bytes());
        assert_eq!(malformed(&first), "two parts overlap");
        first[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes());
        let c = Container::parse(&first).unwrap();
        assert_eq!(c.parts.len(), 1);
        assert_eq!(&c.parts[0].data[16..20], b"PRIV");
    }

    /// Any single corrupted byte of a valid container either fails with an
    /// error or parses into parts that sit past the offset table, inside the
    /// stated size, and claim no byte twice; it never panics.
    #[test]
    fn a_corrupted_byte_anywhere_fails_cleanly_or_parses_soundly() {
        let own = FourCC(*b"PRIV");
        let bytes = build(&[
            (FourCC::SFI0, &[1; 8]),
            (own, &[2; 3]),
            (FourCC::HASH, &[3; 20]),
        ]);
        for i in 0..bytes.len() {
            for value in [0x00, 0x01, 0x7F, 0x80, 0xFF, bytes[i] ^ 0x10] {
                let mut bad = bytes.clone();
                bad[i] = value;
                let Ok(c) = Container::parse(&bad) else {
                    continue;
                };
                let total = u32_at(&bad, 0x18).unwrap() as usize;
                let table_end = HEADER_SIZE + 4 * c.parts.len();
                let mut claimed = vec![false; total];
                for p in &c.parts {
                    let end = p.offset + 8 + p.data.len();
                    assert!(
                        p.offset >= table_end && end <= total,
                        "byte {i} = {value:#x}"
                    );
                    assert_eq!(&bad[p.offset..p.offset + 4], &p.fourcc.0);
                    for b in &mut claimed[p.offset..end] {
                        assert!(!*b, "byte {i} = {value:#x}: overlap");
                        *b = true;
                    }
                }
            }
        }
    }

    /// The stated size bounds a part's header as it bounds its data: a size
    /// word the stated size cuts is truncated though the buffer holds it,
    /// and bytes inside the stated size that no part claims are allowed.
    #[test]
    fn the_stated_size_cuts_a_part_header_whatever_the_buffer_holds() {
        let bytes = build(&[(FourCC::SFI0, &[])]);
        let total = bytes.len() as u32;
        assert_eq!(total as usize, HEADER_SIZE + 4 + 8);
        // One to four bytes off the end leave the FourCC and cut the size
        // word; the buffer still has every byte of it.
        for cut in 1..=4u32 {
            let mut bad = bytes.clone();
            bad[0x18..0x1C].copy_from_slice(&(total - cut).to_le_bytes());
            assert_eq!(malformed(&bad), "truncated", "{cut} bytes cut");
        }
        // Five cut the FourCC itself.
        let mut bad = bytes.clone();
        bad[0x18..0x1C].copy_from_slice(&(total - 5).to_le_bytes());
        assert_eq!(malformed(&bad), "part header out of bounds");
        // No parts, and a stated size covering bytes past the header: the
        // bytes are the container's but no part's.
        let mut loose = build(&[]);
        loose.extend_from_slice(b"PRIV\0\0\0\0");
        loose[0x18..0x1C].copy_from_slice(&(HEADER_SIZE as u32 + 8).to_le_bytes());
        assert!(Container::parse(&loose).unwrap().parts.is_empty());
        // A part followed by unclaimed bytes up to the stated size parses
        // with its own data only.
        let mut tail = build(&[(FourCC::SFI0, &[1; 4])]);
        tail.extend_from_slice(&[0xCC; 8]);
        let len = tail.len() as u32;
        tail[0x18..0x1C].copy_from_slice(&len.to_le_bytes());
        let c = Container::parse(&tail).unwrap();
        assert_eq!(c.part(FourCC::SFI0).unwrap().data, &[1; 4]);
    }
}
