//! The body of the `record` fuzz target, for the mount's record layer
//! (`purg_auth::record`) and the server's proof that rides on it
//! (`purg_auth::server`). `cargo test -p purg-auth --test fuzz_smoke`
//! includes this file too, so the checks run without libFuzzer.
//!
//! It panics only when the record layer or the proof check breaks one of
//! its promises.

use purg_auth::proof::Policy;
use purg_auth::record::{self, Opener, Sealer};
use purg_auth::server;
use purg_auth::session::Keys;

const KEY: [u8; 32] = [7; 32];
/// The largest plaintext the receiver takes, as an msize would bound it.
const MAX: usize = 8216;

/// Arbitrary bytes as a stream of records, then as plaintext.
pub fn record(data: &[u8]) {
    open_stream(data);
    round_trip(data);
    server_proof(data);
}

/// The server key the proofs below are made with.
const HOST: [u8; 32] = [9; 32];

/// Arbitrary bytes as a server's proof are refused without a panic,
/// whether taken whole or after a genuine proof's first byte; a genuine
/// proof for these keys checks, and with one byte changed (chosen by the
/// data) it does not, nor does it check for another session's keys.
fn server_proof(data: &[u8]) {
    let keys = Keys::derive(&[3; 32], data);
    let host = [purg_auth::public_key(&HOST)];
    let policy = Policy {
        keys: &host,
        signers: &host,
        revoked: Some(&[]),
        now: 0,
    };
    assert!(
        server::check(&policy, &keys, data).is_err(),
        "noise checked"
    );
    let _ = server::key(data);
    let good = server::make(&HOST, None, &keys);
    assert_eq!(server::check(&policy, &keys, &good), Ok(()));
    let other = Keys::derive(&[4; 32], data);
    assert!(server::check(&policy, &other, &good).is_err());
    if let Some(&b) = data.first() {
        let mut bad = good.clone();
        let i = usize::from(b) % bad.len();
        bad[i] ^= b | 1;
        assert!(
            server::check(&policy, &keys, &bad).is_err(),
            "a changed proof checked"
        );
        let mut spliced = good[..1].to_vec();
        spliced.extend_from_slice(data);
        let _ = server::check(&policy, &keys, &spliced);
    }
}

/// Bytes nobody sealed: whatever `size` accepts is the record's length,
/// what opens has the right length, and after one failure nothing opens.
fn open_stream(data: &[u8]) {
    let mut o = Opener::new(&KEY);
    let mut rest = data;
    let mut failed = false;
    while let Some(hdr) = rest.first_chunk::<{ record::HEADER }>() {
        let Ok(n) = record::size(*hdr, MAX) else {
            break;
        };
        assert!(n >= record::OVERHEAD && n - record::OVERHEAD <= MAX);
        let Some(rec) = rest.get(..n) else {
            break;
        };
        let got = o.open(rec.to_vec());
        if failed {
            assert!(got.is_err(), "a record opened after a failure");
        }
        if let Ok(plain) = &got {
            assert_eq!(plain.len(), n - record::OVERHEAD);
        }
        failed |= got.is_err();
        rest = &rest[n..];
    }
}

/// `data` cut into messages, sealed and opened again, comes back whole;
/// one changed byte, chosen by the data, makes its record fail.
fn round_trip(data: &[u8]) {
    let mut s = Sealer::new(&KEY);
    let mut o = Opener::new(&KEY);
    let step = data.first().map_or(1, |b| usize::from(*b) + 1);
    let msgs: Vec<&[u8]> = data.chunks(step).collect();
    let recs: Vec<Vec<u8>> = msgs.iter().map(|m| s.seal(m).expect("seal")).collect();
    for (m, r) in msgs.iter().zip(&recs) {
        assert_eq!(r.len(), m.len() + record::OVERHEAD);
        assert_eq!(o.open(r.clone()).expect("a sealed record opens"), *m);
    }
    if let (Some(first), Some(&b)) = (recs.first(), data.last()) {
        let mut bad = first.clone();
        let i = usize::from(b) % bad.len();
        bad[i] ^= b | 1;
        assert!(
            Opener::new(&KEY).open(bad).is_err(),
            "a changed record opened"
        );
        // Out of order.
        if let Some(second) = recs.get(1) {
            assert!(Opener::new(&KEY).open(second.clone()).is_err());
        }
    }
}
