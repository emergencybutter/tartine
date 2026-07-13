//! CRC32C (Castagnoli), hand-rolled table-based implementation.
//!
//! DESIGN.md §4.1 notes production checksums use the kernel's own
//! `crc32c()` (already hardware-accelerated). This crate has no kernel
//! to borrow that from, and pulling in a crate for one well-defined,
//! bounded algorithm isn't worth a new dependency — the table is built
//! at compile time via a `const fn`, so this costs nothing at runtime
//! beyond the table lookup itself.

const POLY: u32 = 0x82f6_3b78; // reversed Castagnoli polynomial

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

const TABLE: [u32; 256] = build_table();

pub fn checksum(data: &[u8]) -> u32 {
    let mut crc: u32 = !0;
    for &b in data {
        crc = TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_standard_check_value() {
        // The canonical CRC32C ("CRC-32/ISCSI") check value for the
        // ASCII string "123456789" — every implementation of this
        // algorithm is expected to reproduce it.
        assert_eq!(checksum(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(checksum(b""), 0);
    }
}
