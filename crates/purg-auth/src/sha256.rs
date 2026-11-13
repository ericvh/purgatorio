//! SHA-256 with the CPU's SHA-256 instructions where the platform says
//! they are there.
//!
//! The module loader takes the digest of every module it loads
//! ([`crate::modsig::inspect`]), and the portable code is most of a
//! kernel spawn (`docs/perf.md` Section 9.2). On aarch64 with FEAT_SHA256
//! the `SHA256H`, `SHA256H2`, `SHA256SU0` and `SHA256SU1` instructions do
//! the compression function several times faster.
//!
//! Detection is the platform's job, since only it can ask the CPU: the
//! kernel and the probe read `ID_AA64ISAR0_EL1` at boot, the hosted emu
//! asks std. Each calls [`use_hardware`] once if the instructions are
//! there. Until then, and on every other target, [`Sha256`] is the `sha2`
//! crate's portable code. Both give the same digests; the tests check
//! one against the other.

#[cfg(target_arch = "aarch64")]
use core::sync::atomic::{AtomicBool, Ordering};

/// A digest.
pub const LEN: usize = 32;

#[cfg(target_arch = "aarch64")]
static HW: AtomicBool = AtomicBool::new(false);

/// Uses the SHA-256 instructions from now on.
///
/// # Safety
///
/// Every CPU this code can run on must implement FEAT_SHA256
/// (`ID_AA64ISAR0_EL1.SHA2` at least 1). On one that does not, the first
/// digest takes an undefined-instruction exception.
#[cfg(target_arch = "aarch64")]
pub unsafe fn use_hardware() {
    HW.store(true, Ordering::Release);
}

/// Whether [`Sha256`] uses the SHA-256 instructions.
pub fn hardware() -> bool {
    #[cfg(target_arch = "aarch64")]
    return HW.load(Ordering::Acquire);
    #[cfg(not(target_arch = "aarch64"))]
    false
}

/// An incremental SHA-256.
#[derive(Clone)]
pub struct Sha256(Inner);

#[derive(Clone)]
enum Inner {
    Soft(sha2::Sha256),
    #[cfg(target_arch = "aarch64")]
    Hw(hw::Hasher),
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    pub fn new() -> Sha256 {
        #[cfg(target_arch = "aarch64")]
        if hardware() {
            return Sha256(Inner::Hw(hw::Hasher::new()));
        }
        Sha256(Inner::Soft(sha2::Digest::new()))
    }

    pub fn update(&mut self, data: &[u8]) {
        match &mut self.0 {
            Inner::Soft(h) => sha2::Digest::update(h, data),
            #[cfg(target_arch = "aarch64")]
            Inner::Hw(h) => h.update(data),
        }
    }

    pub fn finalize(self) -> [u8; LEN] {
        match self.0 {
            Inner::Soft(h) => sha2::Digest::finalize(h).into(),
            #[cfg(target_arch = "aarch64")]
            Inner::Hw(h) => h.finalize(),
        }
    }
}

/// The digest of `data`.
pub fn digest(data: &[u8]) -> [u8; LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// FIPS 180-4's SHA-256 over the aarch64 SHA-256 instructions: the
/// padding and buffering here, the compression function in four
/// instructions per four rounds.
#[cfg(target_arch = "aarch64")]
pub(crate) mod hw {
    use core::arch::aarch64::*;

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    #[derive(Clone)]
    pub struct Hasher {
        state: [u32; 8],
        buf: [u8; 64],
        /// Bytes in `buf`, always below 64.
        len: usize,
        /// Bytes hashed so far.
        total: u64,
    }

    impl Hasher {
        pub fn new() -> Hasher {
            Hasher {
                state: H0,
                buf: [0; 64],
                len: 0,
                total: 0,
            }
        }

        pub fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.len > 0 {
                let n = (64 - self.len).min(data.len());
                self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
                self.len += n;
                data = &data[n..];
                if self.len < 64 {
                    return;
                }
                let b = self.buf;
                compress(&mut self.state, &[b]);
                self.len = 0;
            }
            let (blocks, rest) = data.as_chunks::<64>();
            compress(&mut self.state, blocks);
            self.buf[..rest.len()].copy_from_slice(rest);
            self.len = rest.len();
        }

        pub fn finalize(mut self) -> [u8; 32] {
            let bits = self.total.wrapping_mul(8);
            self.buf[self.len] = 0x80;
            self.buf[self.len + 1..].fill(0);
            if self.len >= 56 {
                let b = self.buf;
                compress(&mut self.state, &[b]);
                self.buf = [0; 64];
            }
            self.buf[56..].copy_from_slice(&bits.to_be_bytes());
            let b = self.buf;
            compress(&mut self.state, &[b]);
            let mut out = [0u8; 32];
            for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.state) {
                *o = w.to_be_bytes();
            }
            out
        }
    }

    fn compress(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
        if blocks.is_empty() {
            return;
        }
        // SAFETY: a `Hasher` exists only once `use_hardware` was called,
        // whose caller promised the instructions are there.
        unsafe { compress_sha2(state, blocks) }
    }

    /// # Safety
    ///
    /// The CPU must implement FEAT_SHA256.
    #[target_feature(enable = "sha2")]
    unsafe fn compress_sha2(state: &mut [u32; 8], blocks: &[[u8; 64]]) {
        // SAFETY: every load and store is within `state`, `K` or the
        // block; the instructions are there (the caller's promise).
        unsafe {
            let mut abcd = vld1q_u32(state.as_ptr());
            let mut efgh = vld1q_u32(state.as_ptr().add(4));
            for block in blocks {
                let (abcd0, efgh0) = (abcd, efgh);
                // The message words are big-endian.
                let w = |i: usize| vreinterpretq_u32_u8(vrev32q_u8(vld1q_u8(block[i..].as_ptr())));
                let mut s = [w(0), w(16), w(32), w(48)];
                for g in 0..16 {
                    let k = g % 4;
                    let t = vaddq_u32(s[k], vld1q_u32(K.as_ptr().add(4 * g)));
                    let prev = abcd;
                    abcd = vsha256hq_u32(prev, efgh, t);
                    efgh = vsha256h2q_u32(efgh, prev, t);
                    if g < 12 {
                        // The next four words of the schedule, for group
                        // g + 4.
                        let x = vsha256su0q_u32(s[k], s[(k + 1) % 4]);
                        s[k] = vsha256su1q_u32(x, s[(k + 2) % 4], s[(k + 3) % 4]);
                    }
                }
                abcd = vaddq_u32(abcd, abcd0);
                efgh = vaddq_u32(efgh, efgh0);
            }
            vst1q_u32(state.as_mut_ptr(), abcd);
            vst1q_u32(state.as_mut_ptr().add(4), efgh);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[cfg(target_arch = "aarch64")]
    use alloc::vec::Vec;

    use super::*;

    fn hex(d: &[u8]) -> alloc::string::String {
        crate::hex::encode(d)
    }

    /// FIPS 180-4's examples and the NIST short-message vectors.
    const VECTORS: &[(&[u8], &str)] = &[
        (
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
        ),
    ];

    const MILLION_A: &str = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";

    #[cfg(target_arch = "aarch64")]
    fn bytes(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    fn soft(data: &[u8]) -> [u8; LEN] {
        use sha2::Digest as _;
        sha2::Sha256::digest(data).into()
    }

    #[test]
    fn known_vectors() {
        for (msg, want) in VECTORS {
            assert_eq!(hex(&digest(msg)), *want);
            assert_eq!(hex(&soft(msg)), *want);
        }
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(hex(&h.finalize()), MILLION_A);
    }

    #[cfg(target_arch = "aarch64")]
    fn have_sha2() -> bool {
        std::arch::is_aarch64_feature_detected!("sha2")
    }

    /// The hardware hasher against the portable code, for lengths around
    /// the block and padding boundaries, fed whole and in pieces.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hardware_matches_portable() {
        if !have_sha2() {
            std::eprintln!("no FEAT_SHA256 here; skipped");
            return;
        }
        let one = |data: &[u8], piece: usize| {
            let mut h = hw::Hasher::new();
            for c in data.chunks(piece.max(1)) {
                h.update(c);
            }
            h.finalize()
        };
        for (msg, want) in VECTORS {
            assert_eq!(hex(&one(msg, msg.len())), *want);
        }
        let mut h = hw::Hasher::new();
        for _ in 0..1000 {
            h.update(&[b'a'; 1000]);
        }
        assert_eq!(hex(&h.finalize()), MILLION_A);
        for n in (0..=200).chain([255, 256, 511, 512, 513, 4096, 65_537]) {
            let data = bytes(n, n as u32 + 7);
            let want = soft(&data);
            for piece in [n, 1, 3, 55, 56, 63, 64, 65, 200] {
                assert_eq!(one(&data, piece), want, "{n} bytes in pieces of {piece}");
            }
        }
    }

    /// `use_hardware` switches `Sha256` over, with the same digests.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn switches_to_hardware() {
        if !have_sha2() {
            return;
        }
        // SAFETY: checked just above.
        unsafe { use_hardware() };
        assert!(hardware());
        assert!(matches!(Sha256::new().0, Inner::Hw(_)));
        let data = bytes(10_000, 1);
        assert_eq!(digest(&data), soft(&data));
    }
}
