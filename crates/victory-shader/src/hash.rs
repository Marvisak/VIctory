//! The multiply-rotate hasher behind the maps translation runs hot: the
//! default's SipHash spends a measurable slice of translation hashing
//! registers and types (`structure` measured it first for its atom maps).

use std::hash::BuildHasherDefault;

/// FxHash's multiply-rotate step.
#[derive(Default)]
pub struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

/// A [`HashMap`](std::collections::HashMap) hashed with [`Fx`].
pub type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fx>>;

/// A [`HashSet`](std::collections::HashSet) hashed with [`Fx`].
pub type HashSet<K> = std::collections::HashSet<K, BuildHasherDefault<Fx>>;
