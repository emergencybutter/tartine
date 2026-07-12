//! FNV-1a: chosen over `std`'s SipHash (unavailable in `core`) and over
//! a crate dependency (this crate has none, by design — see the crate
//! root doc comment) for placement key hashing. It doesn't need to be
//! cryptographically strong, just well-distributed and cheap; FNV-1a is
//! the standard choice when those are the only requirements and you're
//! hand-rolling it.

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub struct Fnv1a(u64);

impl Fnv1a {
    pub const fn new() -> Self {
        Fnv1a(FNV_OFFSET)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv1a {
    fn default() -> Self {
        Self::new()
    }
}

/// Hashes a 128-bit id together with a 64-bit key — the placement
/// scoring function's only hashing need (`disk_id || (inode, chunk_seq)`).
pub fn hash_u128_u64(a: u128, b: u64) -> u64 {
    let mut h = Fnv1a::new();
    h.write(&a.to_le_bytes());
    h.write(&b.to_le_bytes());
    h.finish()
}
