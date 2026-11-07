//! loom models of `Rendez` and the qio `Queue`. Run with
//! `RUSTFLAGS="--cfg loom" cargo test -p purg-core --features std --test loom --release`.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;
use purg_core::qio::Queue;
use purg_platform::sync::Rendez;

#[test]
fn rendez_wakeup_is_never_missed() {
    loom::model(|| {
        let r = Arc::new(Rendez::new());
        let ready = Arc::new(AtomicBool::new(false));
        let waker = {
            let (r, ready) = (r.clone(), ready.clone());
            thread::spawn(move || {
                ready.store(true, Ordering::Release);
                r.wakeup();
            })
        };
        r.sleep(|| ready.load(Ordering::Acquire));
        waker.join().unwrap();
    });
}

#[test]
fn queue_write_larger_than_limit() {
    loom::model(|| {
        let q = Arc::new(Queue::new(2));
        let writer = {
            let q = q.clone();
            thread::spawn(move || q.write(&[1, 2, 3]).unwrap())
        };
        let mut got = Vec::new();
        let mut buf = [0u8; 4];
        while got.len() < 3 {
            let n = q.read(&mut buf);
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(writer.join().unwrap(), 3);
        assert_eq!(got, [1, 2, 3]);
    });
}

#[test]
fn hangup_wakes_a_blocked_reader() {
    loom::model(|| {
        let q = Arc::new(Queue::new(4));
        let closer = {
            let q = q.clone();
            thread::spawn(move || {
                q.write(&[9]).unwrap();
                q.hangup();
            })
        };
        let mut buf = [0u8; 4];
        let mut got = Vec::new();
        loop {
            let n = q.read(&mut buf);
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        closer.join().unwrap();
        assert_eq!(got, [9]);
        assert!(q.write(&[1]).is_err());
    });
}
