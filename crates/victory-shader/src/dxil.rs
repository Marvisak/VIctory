//! The DXIL program inside a container's `DXIL` (or `STAT`) part.
//!
//! ```text
//! 0x00  program version u32   (kind << 16 | major << 4 | minor)
//! 0x04  size in dwords u32
//! 0x08  "DXIL"                <- bitcode offset is relative to here
//! 0x0C  dxil version u32
//! 0x10  bitcode offset u32    (16 in standard PC DXIL)
//! 0x14  bitcode size u32
//! ```

use crate::ShaderError;
use crate::dxbc::u32_at;

const LLVM_BITCODE_MAGIC: [u8; 4] = [0x42, 0x43, 0xC0, 0xDE];
#[cfg(test)]
const STANDARD_BITCODE_OFFSET: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderKind {
    Pixel,
    Vertex,
    Geometry,
    Hull,
    Domain,
    Compute,
    Library,
    RayGeneration,
    Intersection,
    AnyHit,
    ClosestHit,
    Miss,
    Callable,
    Mesh,
    Amplification,
    Node,
    Unknown(u16),
}

impl ShaderKind {
    fn from_raw(v: u16) -> Self {
        use ShaderKind::*;
        match v {
            0 => Pixel,
            1 => Vertex,
            2 => Geometry,
            3 => Hull,
            4 => Domain,
            5 => Compute,
            6 => Library,
            7 => RayGeneration,
            8 => Intersection,
            9 => AnyHit,
            10 => ClosestHit,
            11 => Miss,
            12 => Callable,
            13 => Mesh,
            14 => Amplification,
            15 => Node,
            other => Unknown(other),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DxilProgram<'a> {
    pub kind: ShaderKind,
    pub shader_model: (u8, u8),
    pub dxil_version: u32,
    /// LLVM bitcode (starts with `BC C0 DE`).
    pub bitcode: &'a [u8],
    /// Bytes between the bitcode header and the bitcode. Empty on PC.
    pub prefix: &'a [u8],
    /// Bytes after the bitcode, up to the program size. Empty on PC.
    pub trailer: &'a [u8],
}

/// Xbox additions recognised around the bitcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XboxExtras {
    /// The prefix starts with an `XDAT` table.
    pub xdat: bool,
    /// The trailer contains an `xbop` blob (compiler options).
    pub xbop: bool,
}

impl<'a> DxilProgram<'a> {
    pub fn parse(part: &'a [u8]) -> Result<Self, ShaderError> {
        let version = u32_at(part, 0)?;
        let size = (u32_at(part, 4)? as usize)
            .checked_mul(4)
            .filter(|&s| s <= part.len() && s >= 24)
            .ok_or(ShaderError::Malformed(
                "program size disagrees with part size",
            ))?;
        let part = &part[..size];
        // DXIL starts at shader model 6.0; anything older is DXBC bytecode,
        // so a lower model in a DXIL header means it was misread or forged.
        let shader_model = (((version >> 4) & 0xF) as u8, (version & 0xF) as u8);
        if shader_model.0 < 6 {
            return Err(ShaderError::Malformed("DXIL program below shader model 6"));
        }
        if part.get(8..12) != Some(b"DXIL") {
            return Err(ShaderError::Malformed("missing DXIL magic"));
        }
        let dxil_version = u32_at(part, 12)?;
        // The version is `major << 8 | minor` and DXIL began at 1.0 with
        // shader model 6.0; a zero major is no DXIL any compiler wrote.
        if dxil_version >> 8 == 0 {
            return Err(ShaderError::Malformed("DXIL version below 1.0"));
        }
        let offset = u32_at(part, 16)?;
        let len = u32_at(part, 20)? as usize;
        let start = 8 + offset as usize;
        let bitcode = part
            .get(start..start + len)
            .ok_or(ShaderError::Malformed("bitcode out of bounds"))?;
        if bitcode.get(0..4) != Some(&LLVM_BITCODE_MAGIC) {
            return Err(ShaderError::Malformed("bitcode lacks LLVM magic"));
        }
        // LLVM reads bitcode as 32-bit words and its writer pads to one
        // ("Bitcode stream should be a multiple of 4 bytes in length"); a
        // ragged size cuts the last word short.
        if !len.is_multiple_of(4) {
            return Err(ShaderError::Malformed("bitcode size not a multiple of 4"));
        }
        let prefix = part
            .get(24..start)
            .ok_or(ShaderError::Malformed("bitcode offset too small"))?;
        Ok(DxilProgram {
            kind: ShaderKind::from_raw((version >> 16) as u16),
            shader_model,
            dxil_version,
            bitcode,
            prefix,
            trailer: &part[start + len..],
        })
    }

    /// True for a program laid out exactly like PC DXIL.
    pub fn is_standard_layout(&self) -> bool {
        self.prefix.is_empty() && self.trailer.is_empty()
    }

    pub fn xbox_extras(&self) -> XboxExtras {
        XboxExtras {
            xdat: self.prefix.starts_with(b"XDAT"),
            xbop: self.trailer.windows(4).any(|w| w == b"xbop"),
        }
    }
}

/// Builds a standard PC-layout program around `bitcode` (test helper).
#[cfg(test)]
pub fn build_standard(kind: u16, sm: (u8, u8), dxil_version: u32, bitcode: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let version = (u32::from(kind) << 16) | (u32::from(sm.0) << 4) | u32::from(sm.1);
    let padded = bitcode.len().next_multiple_of(4);
    let size_dwords = (24 + padded) / 4;
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(size_dwords as u32).to_le_bytes());
    out.extend_from_slice(b"DXIL");
    out.extend_from_slice(&dxil_version.to_le_bytes());
    out.extend_from_slice(&STANDARD_BITCODE_OFFSET.to_le_bytes());
    out.extend_from_slice(&(bitcode.len() as u32).to_le_bytes());
    out.extend_from_slice(bitcode);
    out.resize(24 + padded, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bitcode() -> Vec<u8> {
        let mut b = LLVM_BITCODE_MAGIC.to_vec();
        b.extend_from_slice(&[0xAA; 12]);
        b
    }

    #[test]
    fn parses_standard_layout() {
        let bytes = build_standard(0, (6, 0), 0x100, &fake_bitcode());
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.kind, ShaderKind::Pixel);
        assert_eq!(p.shader_model, (6, 0));
        assert!(p.is_standard_layout());
        assert_eq!(p.xbox_extras(), XboxExtras::default());
    }

    #[test]
    fn detects_xbox_layout() {
        // Xbox layout: XDAT table before the bitcode, xbop blob after it.
        let bitcode = fake_bitcode();
        let xdat = b"XDAT\x01\x00\x00\x00";
        let trailer = b"xbop\x02\x00\x00\x00";
        let mut bytes = Vec::new();
        let size = 24 + xdat.len() + bitcode.len() + trailer.len();
        bytes.extend_from_slice(&((5u32 << 16) | 0x65).to_le_bytes()); // cs_6_5
        bytes.extend_from_slice(&((size / 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"DXIL");
        bytes.extend_from_slice(&0x105u32.to_le_bytes());
        bytes.extend_from_slice(&(16 + xdat.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(bitcode.len() as u32).to_le_bytes());
        bytes.extend_from_slice(xdat);
        bytes.extend_from_slice(&bitcode);
        bytes.extend_from_slice(trailer);

        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.kind, ShaderKind::Compute);
        assert_eq!(p.shader_model, (6, 5));
        assert!(!p.is_standard_layout());
        assert_eq!(
            p.xbox_extras(),
            XboxExtras {
                xdat: true,
                xbop: true
            }
        );
        assert_eq!(p.bitcode, &bitcode[..]);
    }

    #[test]
    fn rejects_missing_bitcode_magic() {
        let bytes = build_standard(0, (6, 0), 0x100, &[0u8; 16]);
        assert!(DxilProgram::parse(&bytes).is_err());
    }

    fn malformed(bytes: &[u8]) -> &'static str {
        match DxilProgram::parse(bytes) {
            Err(ShaderError::Malformed(why)) => why,
            other => panic!("expected a malformed program, got {other:?}"),
        }
    }

    fn set_u32(bytes: &mut [u8], at: usize, v: u32) {
        bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn bytes_past_the_program_size_are_not_its_trailer() {
        // A part may be padded beyond the program; only the program's own
        // bytes count, so a PC program stays standard however it is padded.
        let mut bytes = build_standard(1, (6, 6), 0x106, &fake_bitcode());
        bytes.extend_from_slice(b"xbop\0\0\0\0");
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.kind, ShaderKind::Vertex);
        assert_eq!(p.shader_model, (6, 6));
        assert_eq!(p.dxil_version, 0x106);
        assert!(p.trailer.is_empty());
        assert!(p.is_standard_layout());
        assert_eq!(p.xbox_extras(), XboxExtras::default());
    }

    #[test]
    fn unknown_kinds_are_kept() {
        let bytes = build_standard(0x40, (6, 8), 0x108, &fake_bitcode());
        assert_eq!(
            DxilProgram::parse(&bytes).unwrap().kind,
            ShaderKind::Unknown(0x40)
        );
    }

    #[test]
    fn rejects_a_size_the_part_cannot_hold() {
        let good = build_standard(0, (6, 0), 0x100, &fake_bitcode());

        let mut bytes = good.clone();
        set_u32(&mut bytes, 4, (good.len() / 4 + 1) as u32);
        assert_eq!(malformed(&bytes), "program size disagrees with part size");

        // Smaller than the header itself.
        let mut bytes = good.clone();
        set_u32(&mut bytes, 4, 5);
        assert_eq!(malformed(&bytes), "program size disagrees with part size");

        // Large enough to overflow when counted in bytes.
        let mut bytes = good;
        set_u32(&mut bytes, 4, u32::MAX);
        assert_eq!(malformed(&bytes), "program size disagrees with part size");

        assert!(DxilProgram::parse(&[0; 4]).is_err(), "truncated header");
    }

    #[test]
    fn rejects_a_missing_dxil_magic() {
        let mut bytes = build_standard(0, (6, 0), 0x100, &fake_bitcode());
        bytes[8..12].copy_from_slice(b"DXBC");
        assert_eq!(malformed(&bytes), "missing DXIL magic");
    }

    #[test]
    fn rejects_bitcode_past_the_program() {
        let bitcode = fake_bitcode();
        let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
        set_u32(&mut bytes, 20, bitcode.len() as u32 + 4);
        assert_eq!(malformed(&bytes), "bitcode out of bounds");

        let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
        set_u32(&mut bytes, 16, u32::MAX);
        assert_eq!(malformed(&bytes), "bitcode out of bounds");
    }

    #[test]
    fn rejects_bitcode_cut_mid_word() {
        // The last word's bytes stay in the program; only the size is short.
        let bitcode = fake_bitcode();
        for cut in 1..4 {
            let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
            set_u32(&mut bytes, 20, (bitcode.len() - cut) as u32);
            assert_eq!(malformed(&bytes), "bitcode size not a multiple of 4");
        }
        // A whole word fewer is still a word-aligned (if short) stream.
        let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
        set_u32(&mut bytes, 20, (bitcode.len() - 4) as u32);
        assert_eq!(DxilProgram::parse(&bytes).unwrap().bitcode.len(), 12);
    }

    #[test]
    fn rejects_bitcode_inside_the_header() {
        // An offset of 4 puts the bitcode on the DXIL version field. With
        // LLVM's magic written there, only the offset check stops the
        // header being read as bitcode.
        let mut bytes = build_standard(0, (6, 0), 0x100, &fake_bitcode());
        bytes[12..16].copy_from_slice(&LLVM_BITCODE_MAGIC);
        set_u32(&mut bytes, 16, 4);
        assert_eq!(malformed(&bytes), "bitcode offset too small");
    }

    #[test]
    fn rejects_a_shader_model_below_6() {
        // ps_5_1 and vs_4_0 are DXBC-only models; DXIL never carries them.
        for sm in [(5, 1), (5, 0), (4, 0), (0, 0)] {
            let bytes = build_standard(0, sm, 0x100, &fake_bitcode());
            assert_eq!(malformed(&bytes), "DXIL program below shader model 6");
        }
        // 6.0 is the floor, and later majors are kept for whatever comes next.
        for sm in [(6, 0), (6, 9), (7, 0)] {
            let bytes = build_standard(0, sm, 0x100, &fake_bitcode());
            assert_eq!(DxilProgram::parse(&bytes).unwrap().shader_model, sm);
        }
    }

    #[test]
    fn rejects_a_dxil_version_below_1_0() {
        for v in [0x000, 0x001, 0x0FF] {
            let bytes = build_standard(0, (6, 0), v, &fake_bitcode());
            assert_eq!(malformed(&bytes), "DXIL version below 1.0", "{v:#x}");
        }
        // 1.0 is the floor; later versions, Xbox's 1.5 included, are kept.
        for v in [0x100, 0x105, 0x108, 0x200] {
            let bytes = build_standard(0, (6, 0), v, &fake_bitcode());
            assert_eq!(DxilProgram::parse(&bytes).unwrap().dxil_version, v);
        }
    }

    /// Every kind DXC's `DXIL::ShaderKind` numbers, Pixel (0) to Node (15),
    /// decodes to its own variant; 16 (DXC's `Invalid`) and past are kept
    /// as unknown, the kind field's top value included.
    #[test]
    fn every_known_kind_decodes_and_the_rest_are_unknown() {
        use ShaderKind::*;
        let known = [
            Pixel,
            Vertex,
            Geometry,
            Hull,
            Domain,
            Compute,
            Library,
            RayGeneration,
            Intersection,
            AnyHit,
            ClosestHit,
            Miss,
            Callable,
            Mesh,
            Amplification,
            Node,
        ];
        let kind = |k: u16| {
            DxilProgram::parse(&build_standard(k, (6, 0), 0x100, &fake_bitcode()))
                .unwrap()
                .kind
        };
        for (k, want) in known.into_iter().enumerate() {
            assert_eq!(kind(k as u16), want, "kind {k}");
        }
        for k in [16, 17, 0x7FFF, 0xFFFF] {
            assert_eq!(kind(k), Unknown(k), "kind {k}");
        }
        // The kind sits above the model nibbles: neither leaks into it.
        let bytes = build_standard(0xFFFF, (6, 0xF), 0x100, &fake_bitcode());
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!((p.kind, p.shader_model), (Unknown(0xFFFF), (6, 0xF)));
    }

    /// A program with `prefix` between the header and the bitcode and
    /// `trailer` after it.
    fn around(prefix: &[u8], trailer: &[u8]) -> Vec<u8> {
        let bitcode = fake_bitcode();
        let size = 24 + prefix.len() + bitcode.len() + trailer.len();
        assert!(size.is_multiple_of(4));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((5u32 << 16) | 0x65).to_le_bytes());
        bytes.extend_from_slice(&((size / 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"DXIL");
        bytes.extend_from_slice(&0x105u32.to_le_bytes());
        bytes.extend_from_slice(&(16 + prefix.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(bitcode.len() as u32).to_le_bytes());
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&bitcode);
        bytes.extend_from_slice(trailer);
        bytes
    }

    /// `XDAT` counts only at the start of the prefix and `xbop` anywhere in
    /// the trailer, at any byte offset; each in the other's place, or in
    /// the wrong case, is no Xbox addition, though the layout is still not
    /// PC's.
    #[test]
    fn xbox_extras_are_looked_for_only_where_each_belongs() {
        let extras = |prefix: &[u8], trailer: &[u8]| {
            let bytes = around(prefix, trailer);
            let p = DxilProgram::parse(&bytes).unwrap();
            assert_eq!((p.prefix, p.trailer), (prefix, trailer));
            assert!(!p.is_standard_layout());
            let x = p.xbox_extras();
            (x.xdat, x.xbop)
        };
        assert_eq!(extras(b"XDAT", b"xbop"), (true, true));
        // XDAT past the prefix's first byte, or only in the trailer.
        assert_eq!(extras(b"\0\0\0\0XDAT", b""), (false, false));
        assert_eq!(extras(b"", b"XDAT"), (false, false));
        // xbop at an unaligned offset of the trailer still counts.
        assert_eq!(extras(b"", b"\0xbop\0\0\0"), (false, true));
        // xbop only in the prefix, or in another case.
        assert_eq!(extras(b"xbop", b""), (false, false));
        assert_eq!(extras(b"xdat", b"XBOP"), (false, false));
    }

    /// A zero-length bitcode is in bounds but holds no magic; a program of
    /// exactly the 24-byte header is the smallest size accepted.
    #[test]
    fn an_empty_bitcode_lacks_the_magic() {
        let mut bytes = build_standard(0, (6, 0), 0x100, &[]);
        assert_eq!(bytes.len(), 24);
        assert_eq!(malformed(&bytes), "bitcode lacks LLVM magic");
        // The magic just past the program's end is outside it.
        bytes.extend_from_slice(&LLVM_BITCODE_MAGIC);
        set_u32(&mut bytes, 20, 4);
        assert_eq!(malformed(&bytes), "bitcode out of bounds");
        set_u32(&mut bytes, 4, 7);
        assert_eq!(
            DxilProgram::parse(&bytes).unwrap().bitcode,
            &LLVM_BITCODE_MAGIC
        );
    }

    /// A part too short for the version or the size word is truncated, not
    /// misread as a size: the header is read before anything is checked.
    #[test]
    fn a_part_shorter_than_the_first_two_words_is_truncated() {
        for len in 0..8 {
            assert_eq!(malformed(&vec![0x60; len]), "truncated", "{len} bytes");
        }
        // Eight bytes reach the size word, which 24 bytes cannot satisfy.
        assert_eq!(malformed(&[0; 8]), "program size disagrees with part size");
    }

    /// With every field wrong at once, the checks report in header order:
    /// size, shader model, magic, DXIL version, bitcode bounds, LLVM magic,
    /// bitcode length, then the offset floor. Fixing each in turn exposes
    /// the next.
    #[test]
    fn header_checks_report_in_field_order() {
        let bitcode = fake_bitcode();
        let good = build_standard(0, (6, 0), 0x100, &bitcode);
        let mut bytes = good.clone();
        set_u32(&mut bytes, 4, 1); // size
        set_u32(&mut bytes, 0, 0x50); // shader model 5.0
        bytes[8..12].copy_from_slice(b"DXBC");
        set_u32(&mut bytes, 12, 0x0FF); // DXIL 0.255
        set_u32(&mut bytes, 16, 0x1000); // bitcode past the program

        assert_eq!(malformed(&bytes), "program size disagrees with part size");
        bytes[4..8].copy_from_slice(&good[4..8]);
        assert_eq!(malformed(&bytes), "DXIL program below shader model 6");
        bytes[0..4].copy_from_slice(&good[0..4]);
        assert_eq!(malformed(&bytes), "missing DXIL magic");
        bytes[8..12].copy_from_slice(b"DXIL");
        assert_eq!(malformed(&bytes), "DXIL version below 1.0");
        bytes[12..16].copy_from_slice(&good[12..16]);
        assert_eq!(malformed(&bytes), "bitcode out of bounds");

        // Back in bounds, but on zeroed bytes and with a ragged length:
        // the magic is checked before the length.
        set_u32(&mut bytes, 16, 16);
        set_u32(&mut bytes, 20, bitcode.len() as u32 - 1);
        bytes[24..28].fill(0);
        assert_eq!(malformed(&bytes), "bitcode lacks LLVM magic");
        bytes[24..28].copy_from_slice(&LLVM_BITCODE_MAGIC);
        assert_eq!(malformed(&bytes), "bitcode size not a multiple of 4");
        set_u32(&mut bytes, 20, bitcode.len() as u32);
        assert_eq!(bytes, good);
        assert!(DxilProgram::parse(&bytes).is_ok());
    }

    /// The bitcode is a window into the program, not a copy, and the
    /// prefix, bitcode and trailer tile the program after its header.
    #[test]
    fn prefix_bitcode_and_trailer_tile_the_program() {
        let bytes = around(b"XDAT\x01\0\0\0", b"\0\0\0\0xbop\0\0\0\0");
        let p = DxilProgram::parse(&bytes).unwrap();
        let tiled = [p.prefix, p.bitcode, p.trailer].concat();
        assert_eq!(tiled, &bytes[24..]);
        assert!(std::ptr::eq(p.bitcode.as_ptr(), bytes[32..].as_ptr()));
        assert_eq!(p.bitcode.len(), 16);
    }

    /// `xbop` split across the bitcode's end and the trailer's start is in
    /// neither, so it is not found: only the trailer is searched.
    #[test]
    fn xbop_straddling_the_bitcode_end_is_not_found() {
        let mut bitcode = LLVM_BITCODE_MAGIC.to_vec();
        bitcode.extend_from_slice(b"\0\0xb");
        let trailer = b"op\0\0";
        let size = 24 + bitcode.len() + trailer.len();
        let mut bytes = build_standard(5, (6, 5), 0x105, &bitcode);
        bytes.extend_from_slice(trailer);
        set_u32(&mut bytes, 4, (size / 4) as u32);
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(p.trailer, trailer);
        assert!(bytes.windows(4).any(|w| w == b"xbop"));
        assert_eq!(p.xbox_extras(), XboxExtras::default());
        assert!(!p.is_standard_layout());
    }

    /// What every accepted program satisfies, whatever its bytes: a model
    /// of 6 or later, a DXIL version of 1.0 or later, LLVM's magic at the
    /// bitcode's start and a word-aligned length, and the prefix, bitcode
    /// and trailer tiling the program's own bytes after the header and no
    /// further.
    fn assert_sound(bytes: &[u8], p: &DxilProgram, at: &str) {
        let size = u32_at(bytes, 4).unwrap() as usize * 4;
        assert!(p.shader_model.0 >= 6, "{at}");
        assert!(p.dxil_version >> 8 >= 1, "{at}");
        assert!(p.bitcode.starts_with(&LLVM_BITCODE_MAGIC), "{at}");
        assert!(p.bitcode.len().is_multiple_of(4), "{at}");
        let tiled = [p.prefix, p.bitcode, p.trailer].concat();
        assert_eq!(tiled, &bytes[24..size], "{at}");
        let start = 8 + u32_at(bytes, 16).unwrap() as usize;
        assert!(
            std::ptr::eq(p.bitcode.as_ptr(), bytes[start..].as_ptr()),
            "{at}"
        );
    }

    /// Any single corrupted byte of a valid program, PC or Xbox layout,
    /// either fails with an error or parses soundly; it never panics.
    #[test]
    fn a_corrupted_byte_anywhere_fails_cleanly_or_parses_soundly() {
        let programs = [
            build_standard(0, (6, 0), 0x100, &fake_bitcode()),
            around(b"XDAT\x01\0\0\0", b"\0\0\0\0xbop\0\0\0\0"),
        ];
        for bytes in programs {
            for i in 0..bytes.len() {
                for value in [0x00, 0x01, 0x04, 0x7F, 0x80, 0xFF, bytes[i] ^ 0x10] {
                    let mut bad = bytes.clone();
                    bad[i] = value;
                    if let Ok(p) = DxilProgram::parse(&bad) {
                        assert_sound(&bad, &p, &format!("byte {i} = {value:#x}"));
                    }
                }
            }
        }
    }

    /// A program cut short anywhere is refused: the size word then claims
    /// more than the part holds, before any other field is read, so no cut
    /// reads past the end or names a later check.
    #[test]
    fn a_program_cut_short_anywhere_is_refused() {
        let bytes = around(b"XDAT\x01\0\0\0", b"xbop");
        assert!(DxilProgram::parse(&bytes).is_ok());
        for len in 8..bytes.len() {
            assert_eq!(
                malformed(&bytes[..len]),
                "program size disagrees with part size",
                "{len} of {} bytes",
                bytes.len()
            );
        }
    }

    /// Lowering the size word a dword at a time drops the trailer first,
    /// then cuts into the bitcode, which is then out of bounds: the size
    /// decides the program's end, not the part's length.
    #[test]
    fn a_smaller_size_word_drops_the_trailer_then_cuts_the_bitcode() {
        let trailer = b"\0\0\0\0xbop";
        let mut bytes = around(b"", trailer);
        let full = bytes.len() / 4;
        for drop in 0..=full {
            set_u32(&mut bytes, 4, (full - drop) as u32);
            let kept = trailer.len().saturating_sub(4 * drop);
            let cut = 4 * drop > trailer.len();
            match DxilProgram::parse(&bytes) {
                Ok(p) => {
                    assert!(!cut, "{drop} dwords dropped");
                    assert_eq!(p.trailer, &trailer[..kept]);
                    assert_eq!(p.xbox_extras().xbop, kept == trailer.len());
                    assert_sound(&bytes, &p, &format!("{drop} dwords dropped"));
                }
                Err(ShaderError::Malformed(why)) => {
                    assert!(cut, "{drop} dwords dropped: {why}");
                    let want = if (full - drop) * 4 < 24 {
                        "program size disagrees with part size"
                    } else {
                        "bitcode out of bounds"
                    };
                    assert_eq!(why, want, "{drop} dwords dropped");
                }
                Err(other) => panic!("{other:?}"),
            }
        }
    }

    /// The version word's bits 8-15, between the model nibbles and the
    /// kind, belong to neither: whatever they hold, the kind and model read
    /// the same.
    #[test]
    fn the_version_bits_between_model_and_kind_are_ignored() {
        let base = build_standard(5, (6, 5), 0x105, &fake_bitcode());
        for between in [0x01, 0x80, 0xAB, 0xFF] {
            let mut bytes = base.clone();
            bytes[1] = between;
            let p = DxilProgram::parse(&bytes).unwrap();
            assert_eq!(
                (p.kind, p.shader_model),
                (ShaderKind::Compute, (6, 5)),
                "{between:#x}"
            );
        }
    }

    /// The bitcode offset need not be word-aligned: an offset of 17 puts
    /// the bitcode one byte into the program after the header, leaving a
    /// one-byte prefix and a three-byte trailer that still fill the
    /// program's words, and the bitcode is read from that byte.
    #[test]
    fn an_unaligned_bitcode_offset_leaves_ragged_prefix_and_trailer() {
        let bitcode = fake_bitcode();
        let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
        bytes.insert(24, 0x5A);
        bytes.extend_from_slice(&[0xEE; 3]);
        let words = (bytes.len() / 4) as u32;
        set_u32(&mut bytes, 4, words);
        set_u32(&mut bytes, 16, 17);
        let p = DxilProgram::parse(&bytes).unwrap();
        assert_eq!(
            (p.prefix, p.bitcode, p.trailer),
            (&[0x5A][..], &bitcode[..], &[0xEE; 3][..])
        );
        assert!(std::ptr::eq(p.bitcode.as_ptr(), bytes[25..].as_ptr()));
        assert!(!p.is_standard_layout());
        assert_eq!(p.xbox_extras(), XboxExtras::default());
        assert_sound(&bytes, &p, "offset 17");
    }

    /// A bitcode length at the top of its field, word-aligned or not, is
    /// out of bounds of any program rather than wrapping past it, and so
    /// is a length that would end exactly one byte past the program.
    #[test]
    fn a_bitcode_length_at_the_field_top_is_out_of_bounds() {
        let bitcode = fake_bitcode();
        for len in [u32::MAX, u32::MAX - 3, bitcode.len() as u32 + 1] {
            let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
            set_u32(&mut bytes, 20, len);
            assert_eq!(malformed(&bytes), "bitcode out of bounds", "{len:#x}");
        }
        // Both at their top: offset and length together still only fail.
        let mut bytes = build_standard(0, (6, 0), 0x100, &bitcode);
        set_u32(&mut bytes, 16, u32::MAX);
        set_u32(&mut bytes, 20, u32::MAX);
        assert_eq!(malformed(&bytes), "bitcode out of bounds");
    }

    /// The model nibbles run to 15 each and nothing past 6 is refused; the
    /// DXIL version check looks at every bit above the minor byte, so a
    /// version with only high bits set is no zero major and is kept whole.
    #[test]
    fn model_nibbles_and_dxil_version_bits_reach_their_tops() {
        for sm in [(6, 15), (15, 0), (15, 15)] {
            let bytes = build_standard(0, sm, 0x100, &fake_bitcode());
            assert_eq!(DxilProgram::parse(&bytes).unwrap().shader_model, sm);
        }
        for v in [0x0001_0000, 0x8000_0000, u32::MAX] {
            let bytes = build_standard(0, (6, 0), v, &fake_bitcode());
            assert_eq!(
                DxilProgram::parse(&bytes).unwrap().dxil_version,
                v,
                "{v:#x}"
            );
        }
        // The minor byte alone, at its top, is still below 1.0.
        let bytes = build_standard(0, (6, 0), 0xFF, &fake_bitcode());
        assert_eq!(malformed(&bytes), "DXIL version below 1.0");
    }
}
