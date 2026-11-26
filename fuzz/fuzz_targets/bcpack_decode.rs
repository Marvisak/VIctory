//! Fuzzes `bcpack::decode` with the shipped tables.
//!
//! Input: byte 0 picks the BC format, byte 1 the block count, the rest is
//! the packed stream.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use victory_formats::bcpack::{decode, Tables};

const FORMATS: [u8; 6] = [1, 3, 4, 5, 6, 7];

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables::builtin().expect("the shipped tables parse"))
}

fuzz_target!(|data: &[u8]| {
    let [format, count, src @ ..] = data else { return };
    let bc = FORMATS[usize::from(*format) % FORMATS.len()];
    let block = if matches!(bc, 1 | 4) { 8 } else { 16 };
    let blocks = 1 + usize::from(*count) % 64;
    let mut dst = vec![0u8; block * blocks];
    let _ = decode(bc, src, &mut dst, tables());
});
