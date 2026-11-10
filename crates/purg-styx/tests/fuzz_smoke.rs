//! A fuzz smoke test that needs no tools: the fuzz targets' own checks
//! (`fuzz/src/lib.rs`) run over deterministic pseudo-random mutations of
//! the captured fixtures and of the .u seed conversation. The seed is
//! fixed, so a failure reproduces; the panic names the case.
//!
//! The real fuzzers are in `fuzz/` (docs/plan.md Section 9.3).

use std::fs;
use std::path::PathBuf;

use purg_styx::dotu::{NONUNAME, VERSION_U};
use purg_styx::{DMDIR, Dir, Fcall, Msg, NOFID, NOTAG, QTDIR, Qid, kind};

#[path = "../../../fuzz/src/lib.rs"]
mod harness;

/// Mutated inputs per target.
const CASES: u64 = 4000;
const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The captured 9P2000 traffic, in conversation order.
fn fixtures() -> Vec<Vec<u8>> {
    let mut files: Vec<_> = fs::read_dir(manifest_dir().join("tests/fixtures/9fans"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 34);
    files.iter().map(|p| fs::read(p).unwrap()).collect()
}

/// A little-endian builder for the .u side.
#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn u8(mut self, v: u8) -> W {
        self.0.push(v);
        self
    }
    fn u16(mut self, v: u16) -> W {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> W {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn s(self, s: &str) -> W {
        self.u16(s.len() as u16).bytes(s.as_bytes())
    }
    fn bytes(mut self, b: &[u8]) -> W {
        self.0.extend_from_slice(b);
        self
    }
    fn qid(self, q: Qid) -> W {
        self.u8(q.qtype).u32(q.vers).bytes(&q.path.to_le_bytes())
    }
    fn msg(self, ty: u8, tag: u16) -> Vec<u8> {
        let head = W::default().u32(7 + self.0.len() as u32).u8(ty).u16(tag);
        head.bytes(&self.0).0
    }
}

/// A .u stat entry as QEMU sends it: empty names and numeric ids.
fn ustat(name: &str, qid: Qid, mode: u32, ids: [u32; 3]) -> Vec<u8> {
    let d = Dir {
        qid,
        mode,
        atime: 1_700_000_000,
        mtime: 1_700_000_001,
        length: 5,
        name: name.into(),
        ..Dir::default()
    };
    let w = W(d.pack().unwrap()).s("");
    let mut out = w.u32(ids[0]).u32(ids[1]).u32(ids[2]).0;
    let body = (out.len() - 2) as u16;
    out[..2].copy_from_slice(&body.to_le_bytes());
    out
}

/// A conversation with a 9P2000.u server, as the shim sees it: 9P2000
/// requests and .u replies.
fn u_conversation() -> Vec<Vec<u8>> {
    let enc = |tag, fcall| Msg { tag, fcall }.encode().unwrap();
    let dirq = Qid {
        path: 1,
        vers: 0,
        qtype: QTDIR,
    };
    let fileq = Qid {
        path: 2,
        vers: 3,
        qtype: 0,
    };
    let entries = [
        ustat(".", dirq, DMDIR | 0o755, [0, 0, 0]),
        ustat("..", dirq, DMDIR | 0o755, [0, 0, 0]),
        ustat("sub", dirq, DMDIR | 0o755, [1000, 1000, NONUNAME]),
        ustat("hello.txt", fileq, 0o644, [4_000_000_000, 100, 0]),
    ]
    .concat();
    let stat = ustat("hello.txt", fileq, 0o644, [1000, 1000, 0]);
    let wstat = Dir {
        name: "renamed".into(),
        ..Dir::null()
    };
    vec![
        enc(
            NOTAG,
            Fcall::Tversion {
                msize: 8216,
                version: "9P2000",
            },
        ),
        W::default()
            .u32(8216)
            .s(VERSION_U)
            .msg(kind::RVERSION, NOTAG),
        enc(
            1,
            Fcall::Tauth {
                afid: 9,
                uname: "inferno",
                aname: "",
            },
        ),
        W::default()
            .s("Operation not supported")
            .u32(95)
            .msg(kind::RERROR, 1),
        enc(
            1,
            Fcall::Tattach {
                fid: 1,
                afid: NOFID,
                uname: "inferno",
                aname: "",
            },
        ),
        W::default().qid(dirq).msg(kind::RATTACH, 1),
        enc(
            2,
            Fcall::Twalk {
                fid: 1,
                newfid: 2,
                wnames: vec!["hello.txt"],
            },
        ),
        W::default().u16(1).qid(fileq).msg(kind::RWALK, 2),
        enc(3, Fcall::Topen { fid: 1, mode: 0 }),
        W::default().qid(dirq).u32(0).msg(kind::ROPEN, 3),
        enc(
            4,
            Fcall::Tread {
                fid: 1,
                offset: 0,
                count: 8192,
            },
        ),
        W::default()
            .u32(entries.len() as u32)
            .bytes(&entries)
            .msg(kind::RREAD, 4),
        enc(5, Fcall::Tstat { fid: 2 }),
        W::default().u16(0).bytes(&stat).msg(kind::RSTAT, 5),
        enc(
            6,
            Fcall::Twstat {
                fid: 2,
                stat: &wstat.pack().unwrap(),
            },
        ),
        W::default().msg(kind::RWSTAT, 6),
        enc(
            7,
            Fcall::Tcreate {
                fid: 1,
                name: "new",
                perm: 0o644,
                mode: 1,
            },
        ),
        W::default().s("File exists").u32(17).msg(kind::RERROR, 7),
        enc(
            8,
            Fcall::Twrite {
                fid: 2,
                offset: 0,
                data: b"hello",
            },
        ),
        enc(9, Fcall::Tflush { oldtag: 8 }),
        W::default().msg(kind::RFLUSH, 9),
        enc(10, Fcall::Tremove { fid: 2 }),
        W::default()
            .s("Permission denied")
            .u32(13)
            .msg(kind::RERROR, 10),
        enc(11, Fcall::Tclunk { fid: 1 }),
        W::default().msg(kind::RCLUNK, 11),
    ]
}

/// The seed corpus of the `dotu` fuzz target, as (file name, contents).
fn dotu_seeds() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("conversation-9p2000.bin", fixtures().concat()),
        ("conversation-u.bin", u_conversation().concat()),
    ]
}

/// The seeds in `fuzz/seeds/dotu` are the ones built here. Run with
/// `PURG_BLESS=1` to rewrite them after changing the conversation.
#[test]
fn dotu_seed_corpus_is_current() {
    let dir = manifest_dir().join("../../fuzz/seeds/dotu");
    for (name, bytes) in dotu_seeds() {
        let path = dir.join(name);
        if std::env::var_os("PURG_BLESS").is_some() {
            fs::write(&path, &bytes).unwrap();
        }
        let have = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert!(have == bytes, "{name} is stale; run with PURG_BLESS=1");
    }
}

/// The unmutated seeds pass, and the shim translates every message of both
/// conversations.
#[test]
fn seeds_pass_unmutated() {
    for f in fixtures() {
        harness::decode(&f);
    }
    for conversation in [fixtures(), u_conversation()] {
        let mut shim = purg_styx::dotu::Shim::new();
        for (i, m) in conversation.iter().enumerate() {
            let out = if m[4] % 2 == 0 {
                harness::request(&mut shim, m)
            } else {
                harness::response(&mut shim, m)
            };
            assert!(out.is_some(), "message {i} was refused");
        }
    }
    for (_, seed) in dotu_seeds() {
        harness::dotu(&seed);
    }
}

/// xorshift64*: small, fast, and the same on every host.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Values that sit on the codec's edges.
const EDGES: [u32; 12] = [
    0,
    1,
    6,
    7,
    16,
    17,
    0x7f,
    0xff,
    0xffff,
    0x1_0000,
    0x7fff_ffff,
    0xffff_ffff,
];

fn mutate(rng: &mut Rng, buf: &mut Vec<u8>, seeds: &[Vec<u8>]) {
    match rng.below(9) {
        // Flip a bit.
        0 if !buf.is_empty() => {
            let i = rng.below(buf.len());
            buf[i] ^= 1 << rng.below(8);
        }
        // Replace a byte.
        1 if !buf.is_empty() => {
            let i = rng.below(buf.len());
            buf[i] = rng.next() as u8;
        }
        // Truncate.
        2 => buf.truncate(rng.below(buf.len() + 1)),
        // Insert random bytes.
        3 => {
            let at = rng.below(buf.len() + 1);
            let n = 1 + rng.below(8);
            let new: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            buf.splice(at..at, new);
        }
        // Delete a range.
        4 if !buf.is_empty() => {
            let at = rng.below(buf.len());
            let n = 1 + rng.below((buf.len() - at).min(8));
            buf.drain(at..at + n);
        }
        // Overwrite a 16-bit field with an edge value.
        5 if buf.len() >= 2 => {
            let at = rng.below(buf.len() - 1);
            let v = EDGES[rng.below(EDGES.len())] as u16;
            buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
        }
        // Overwrite a 32-bit field with an edge value.
        6 if buf.len() >= 4 => {
            let at = rng.below(buf.len() - 3);
            let v = EDGES[rng.below(EDGES.len())];
            buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        // Splice in part of another seed.
        7 => {
            let other = &seeds[rng.below(seeds.len())];
            let from = rng.below(other.len() + 1);
            let n = rng.below((other.len() - from).min(64) + 1);
            let at = rng.below(buf.len() + 1);
            buf.splice(at..at, other[from..from + n].iter().copied());
        }
        // Change the message type, keeping the rest.
        _ if buf.len() > 4 => buf[4] = 100 + rng.below(28) as u8,
        _ => {}
    }
}

/// Runs `target` over `CASES` mutations of `seeds`. With `reframe`, half
/// the cases get their `size[4]` set to the mutated length, so that the
/// mutation reaches the message body instead of failing the frame check.
fn smoke(seeds: &[Vec<u8>], reframe: bool, target: fn(&[u8])) {
    let mut rng = Rng(SEED);
    for case in 0..CASES {
        let mut buf = seeds[rng.below(seeds.len())].clone();
        for _ in 0..1 + rng.below(4) {
            mutate(&mut rng, &mut buf, seeds);
        }
        if reframe && buf.len() >= 4 && rng.below(2) == 0 {
            let n = buf.len() as u32;
            buf[..4].copy_from_slice(&n.to_le_bytes());
        }
        let result = std::panic::catch_unwind(|| target(&buf));
        assert!(result.is_ok(), "case {case} panicked on {buf:02x?}");
    }
}

#[test]
fn decoder_survives_mutated_fixtures() {
    let mut seeds = fixtures();
    seeds.extend(u_conversation());
    smoke(&seeds, true, harness::decode);
}

#[test]
fn shim_survives_mutated_messages() {
    let mut seeds = fixtures();
    seeds.extend(u_conversation());
    // One message at a time: requests and replies of every kind, most of
    // them unsolicited.
    smoke(&seeds, true, harness::dotu);
}

#[test]
fn shim_survives_mutated_conversations() {
    // Whole conversations, so that the shim's fid and tag state is in play
    // when the mutated message arrives.
    let seeds: Vec<Vec<u8>> = dotu_seeds().into_iter().map(|(_, b)| b).collect();
    smoke(&seeds, false, harness::dotu);
}
