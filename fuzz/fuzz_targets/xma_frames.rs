//! Fuzzes the XMA packet walk, frame reader and decoder.
//!
//! Input: byte 0 picks channels and sample rate, the rest is the stream.
#![no_main]

use libfuzzer_sys::fuzz_target;
use victory_formats::xma::{packet, read_frame, Decoder, PACKET_BYTES};

/// Packet header size in bits (`HEADER_BITS`, private in `xma.rs`).
const HEADER_BITS: usize = 32;
const PACKET_BITS: usize = PACKET_BYTES * 8;
const RATES: [u32; 4] = [24_000, 32_000, 44_100, 48_000];
/// Bounds the walk in case `next` ever points backwards.
const MAX_FRAMES: usize = 256;

fuzz_target!(|input: &[u8]| {
    let [cfg, data @ ..] = input else { return };
    let channels = 1 + usize::from(cfg & 1);
    let rate = RATES[usize::from(cfg >> 1) % RATES.len()];
    let mut decoder = Decoder::new(channels, rate);
    let mut pcm = Vec::new();

    // Walk every packet the way a stream is read.
    for index in 0..=data.len() / PACKET_BYTES {
        let Some(p) = packet(data, index) else { continue };
        let mut at = Some(index * PACKET_BITS + HEADER_BITS + p.first_frame_bit as usize);
        for _ in 0..MAX_FRAMES {
            let Some(frame) = at.and_then(|bit| read_frame(data, bit)) else { break };
            let _ = decoder.decode(&frame.data, frame.bits, &mut pcm);
            pcm.clear();
            at = frame.next;
        }
    }
    decoder.flush(&mut pcm);

    // A frame read from an arbitrary bit offset must not panic either.
    if let [a, b, c, d, ..] = *data {
        let bit = u32::from_le_bytes([a, b, c, d]) as usize % (data.len() * 8);
        if let Some(frame) = read_frame(data, bit) {
            let _ = decoder.decode(&frame.data, frame.bits, &mut pcm);
        }
    }
});
