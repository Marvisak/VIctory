//! Fuzzes `bcpack::Tables::parse`, both on raw bytes and on the shipped
//! tables with fuzzer-chosen bytes overwritten, then decodes with any
//! tables that still parse.
#![no_main]

use libfuzzer_sys::fuzz_target;
use victory_formats::bcpack::{decode, Tables};

const SHIPPED: &[u8] = include_bytes!("../../crates/victory-formats/src/bcpack-tables.bin");

fuzz_target!(|data: &[u8]| {
    let _ = Tables::parse(data);

    // Each 4-byte chunk is (24-bit offset, new value) into the shipped tables.
    let mut bytes = SHIPPED.to_vec();
    for &[a, b, c, value] in data.as_chunks::<4>().0 {
        let at = u32::from_le_bytes([a, b, c, 0]) as usize % bytes.len();
        bytes[at] = value;
    }
    let Ok(tables) = Tables::parse(&bytes) else { return };

    for (bc, block) in [(1, 8), (3, 16), (4, 8), (5, 16), (6, 16), (7, 16)] {
        let mut dst = vec![0u8; block * 4];
        let _ = decode(bc, data, &mut dst, &tables);
    }
});
