//! The `lethe` fuzz target's checks (`fuzz/src/lethe.rs`) over
//! deterministic pseudo-random inputs: genuine frames and plaintexts of
//! every kind, the same with bytes changed, cut or extended, and noise.
//! The seed is fixed, so a failure reproduces; the panic names the case.

use purg_lethe::msg::{Frame, Grant, Msg, Op, Reply, Request, Tauth};
use purg_styx::{Dir, Qid};

#[path = "../../../fuzz/src/lethe.rs"]
mod lethe;

const CASES: u64 = 2000;

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

/// Genuine inputs: frames the client and server make, and plaintexts.
fn corpus() -> Vec<Vec<u8>> {
    let s = lethe::session();
    let mut v = Vec::new();
    for op in [
        Op::Stat { path: "/f" },
        Op::Read {
            path: "/f",
            offset: 1,
            count: 3,
            have: Some(1),
        },
        Op::Read {
            path: "/f",
            offset: 0,
            count: 100,
            have: None,
        },
        Op::Readdir {
            path: "/",
            cookie: b"",
            count: 500,
        },
        Op::Stat { path: "/nope" },
        Op::Walk {
            path: "/",
            names: vec!["f", "x"],
        },
        Op::Walk {
            path: "/",
            names: vec![],
        },
    ] {
        let (f, _) = s.request(5, lethe::NOW, &op, [6; 24]);
        v.push(lethe::handle(&f));
        v.push(f);
        // The same in the clear: the request, its reply, the bare op.
        let (f, _) = purg_lethe::client::clear(5, &op);
        v.push(lethe::handle_clear(&f));
        v.push(f);
        v.push(op.encode());
        v.push(
            Request {
                time: lethe::NOW,
                op,
            }
            .encode(),
        );
    }
    let creds = purg_lethe::client::Creds {
        user: String::from("u"),
        seed: lethe::SEED,
        cert: None,
    };
    let (auth, _) = purg_lethe::client::auth(&creds, 1, 1, "/", [3; 32]);
    v.push(lethe::handle(&auth));
    v.push(auth);
    let d = Dir {
        name: String::from("f"),
        ..Dir::default()
    };
    let packed = d.pack().unwrap();
    for r in [
        Reply::Stat(d.clone()),
        Reply::Read {
            qid: Qid::default(),
            unchanged: false,
            data: b"abc",
        },
        Reply::Readdir {
            qid: Qid::default(),
            cookie: b"f",
            entries: &packed,
        },
        Reply::Error("permission denied"),
        Reply::Walk {
            qids: vec![Qid::default()],
            error: "",
            stat: Some(d.clone()),
        },
        Reply::Walk {
            qids: vec![],
            error: "file does not exist",
            stat: None,
        },
    ] {
        v.push(r.encode());
    }
    v.push(
        Grant {
            now: 1,
            expires: 2,
            rights: 1,
            prefix: "/",
            ticket: s.ticket(),
        }
        .encode(),
    );
    v.push(s.ticket().to_vec());
    v.push(
        Msg {
            id: 3,
            frame: Frame::Tauth(Tauth {
                eph: [0; 32],
                rights: 0xff,
                prefix: "",
                uname: "",
                kind: 9,
                body: b"",
                signature: [0; 64],
            }),
        }
        .encode(),
    );
    v
}

#[test]
fn lethe_target_holds() {
    let corpus = corpus();
    for c in &corpus {
        lethe::lethe(c);
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..CASES {
        let mut data = corpus[rng.below(corpus.len() as u64) as usize].clone();
        match case % 4 {
            // Bytes changed.
            0 | 1 => {
                for _ in 0..=rng.below(4) {
                    if !data.is_empty() {
                        let i = rng.below(data.len() as u64) as usize;
                        data[i] = rng.next() as u8;
                    }
                }
            }
            // Cut or extended, with the size field left or fixed up.
            2 => {
                let n = rng.below(data.len() as u64 + 8) as usize;
                data.resize(n, rng.next() as u8);
                if rng.below(2) == 0 && data.len() >= 4 {
                    let len = (data.len() as u32).to_le_bytes();
                    data[..4].copy_from_slice(&len);
                }
            }
            // Noise.
            _ => {
                let n = rng.below(200) as usize;
                data = (0..n).map(|_| rng.next() as u8).collect();
            }
        }
        let r = std::panic::catch_unwind(|| lethe::lethe(&data));
        assert!(r.is_ok(), "case {case}: {data:?}");
    }
}
