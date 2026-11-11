//! Checksums: CRC-32C (Castagnoli) for directory entries, and a 64-bit
//! FNV-1a hash, folded to 32 bits, for the superblock and the journal.
//!
//! The outer checksums must not be a CRC over the same polynomial: a CRC
//! over a block of entries that each end in their own CRC-32C does not
//! depend on the entries at all (each entry with its CRC appended adds a
//! constant to the register), so a torn journal of directory blocks would
//! pass. FNV-1a is not linear, so it has no such blind spot.

const POLY: u32 = 0x82f6_3b78;

const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// A running CRC-32C.
#[derive(Clone, Copy, Debug)]
pub struct Crc(u32);

impl Default for Crc {
    fn default() -> Crc {
        Crc::new()
    }
}

impl Crc {
    pub const fn new() -> Crc {
        Crc(!0)
    }

    pub fn update(mut self, data: &[u8]) -> Crc {
        for &b in data {
            self.0 = TABLE[((self.0 ^ u32::from(b)) & 0xff) as usize] ^ (self.0 >> 8);
        }
        self
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    Crc::new().update(data).finish()
}

/// A running 64-bit FNV-1a hash.
#[derive(Clone, Copy, Debug)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv::new()
    }
}

impl Fnv {
    pub const fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(mut self, data: &[u8]) -> Fnv {
        for &b in data {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self
    }

    /// The hash folded to 32 bits.
    pub fn finish(self) -> u32 {
        (self.0 ^ (self.0 >> 32)) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // RFC 3720, B.4.
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(
            Crc::new().update(b"1234").update(b"56789").finish(),
            0xe306_9283
        );
    }

    /// Why the journal does not use a CRC: an entry with its own CRC-32C
    /// appended is invisible to an outer CRC-32C.
    #[test]
    fn outer_hash_sees_inner_checksummed_records() {
        let rec = |fill: u8| {
            let mut r = [fill; 12];
            let c = crc32c(&r[..8]);
            r[8..].copy_from_slice(&c.to_le_bytes());
            r
        };
        let (a, b) = (rec(1), rec(2));
        let outer = |r: &[u8]| Crc::new().update(b"head").update(r).finish();
        assert_eq!(outer(&a), outer(&b));
        let fnv = |r: &[u8]| Fnv::new().update(b"head").update(r).finish();
        assert_ne!(fnv(&a), fnv(&b));
    }
}
