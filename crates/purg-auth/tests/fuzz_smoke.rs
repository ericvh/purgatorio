//! The `record` fuzz target's checks (`fuzz/src/record.rs`) over
//! deterministic pseudo-random inputs: genuine record streams, the same
//! with bytes changed, server proofs (a key or a certificate, whole or
//! changed), and noise. The seed is fixed, so a failure reproduces; the
//! panic names the case.

use purg_auth::cert::Cert;
use purg_auth::record::Sealer;
use purg_auth::session::Keys;

#[path = "../../../fuzz/src/record.rs"]
mod record;

const CASES: u64 = 3000;

/// xorshift64*, enough to spread the cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[test]
fn record_target_holds() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..CASES {
        let mut data = Vec::new();
        match case % 4 {
            // A server's proof, with a certificate or a bare key, then
            // changed in a few places or cut short.
            3 => {
                let keys = Keys::derive(&[case as u8; 32], &case.to_le_bytes());
                let seed = [rng.next() as u8; 32];
                let cert = (rng.below(2) == 0).then(|| {
                    let mut c =
                        Cert::new("host", purg_auth::public_key(&seed), rng.next() >> 1).unwrap();
                    c.sign(&[5; 32]).unwrap();
                    c
                });
                data = purg_auth::server::make(&seed, cert.as_ref(), &keys);
                for _ in 0..rng.below(4) {
                    let i = rng.below(data.len() as u64) as usize;
                    data[i] ^= (rng.next() as u8) | 1;
                }
                if rng.below(4) == 0 {
                    data.truncate(rng.below(data.len() as u64) as usize);
                }
            }
            // Records sealed under the target's key.
            0 | 1 => {
                let mut s = Sealer::new(&[7; 32]);
                for _ in 0..rng.below(6) {
                    let len = rng.below(300) as usize;
                    let msg: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                    data.extend_from_slice(&s.seal(&msg).unwrap());
                }
                if case % 4 == 1 && !data.is_empty() {
                    for _ in 0..=rng.below(3) {
                        let i = rng.below(data.len() as u64) as usize;
                        data[i] ^= (rng.next() as u8) | 1;
                    }
                }
            }
            // Noise.
            _ => {
                let len = rng.below(600) as usize;
                data = (0..len).map(|_| rng.next() as u8).collect();
            }
        }
        let r = std::panic::catch_unwind(|| record::record(&data));
        assert!(r.is_ok(), "case {case}: {data:02x?}");
    }
}
