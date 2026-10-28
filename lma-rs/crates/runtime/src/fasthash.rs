//! A fast non-cryptographic hasher for the render path's maps (the Fx hash rustc uses). The
//! keys are addresses, register words and our own 64-bit content hashes, none chosen by an
//! attacker; std's SipHash default was ~5% of the render thread in a match (#6).

use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline(always)]
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b) ^ ((rest.len() as u64) << 56));
        }
    }
    #[inline(always)]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline(always)]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// `std::collections::HashMap` with `FxHasher` (create with `HashMap::default()`).
pub type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
/// `std::collections::HashSet` with `FxHasher`.
pub type HashSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;
