//! Small, fast non-cryptographic hasher (FxHash) for the many integer-keyed maps.
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl Fx {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(K);
    }
}

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let mut it = bytes.chunks_exact(8);
        for c in &mut it {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let mut last = 0u64;
        for (k, b) in it.remainder().iter().enumerate() {
            last |= (*b as u64) << (8 * k);
        }
        self.add(last ^ (bytes.len() as u64) << 56);
    }
    fn write_u8(&mut self, i: u8) { self.add(i as u64) }
    fn write_u32(&mut self, i: u32) { self.add(i as u64) }
    fn write_u64(&mut self, i: u64) { self.add(i) }
    fn write_usize(&mut self, i: usize) { self.add(i as u64) }
    fn write_u128(&mut self, i: u128) { self.add(i as u64); self.add((i >> 64) as u64) }
    fn write_i128(&mut self, i: i128) { self.write_u128(i as u128) }
    fn write_i64(&mut self, i: i64) { self.add(i as u64) }
    fn finish(&self) -> u64 {
        // splitmix64 finalizer: spread entropy into the low bits hashbrown indexes by
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

pub type Map<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;
pub type Set<K> = HashSet<K, BuildHasherDefault<Fx>>;
