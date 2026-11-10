//! The `record` fuzz target's checks (`fuzz/src/record.rs`) over
//! deterministic pseudo-random inputs: genuine record streams, the same
//! with bytes changed, and noise. The seed is fixed, so a failure
//! reproduces; the panic names the case.

use purg_auth::record::Sealer;

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
        match case % 3 {
            // Records sealed under the target's key.
            0 | 1 => {
                let mut s = Sealer::new(&[7; 32]);
                for _ in 0..rng.below(6) {
                    let len = rng.below(300) as usize;
                    let msg: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                    data.extend_from_slice(&s.seal(&msg).unwrap());
                }
                if case % 3 == 1 && !data.is_empty() {
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
