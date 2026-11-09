//! loom models of `Rendez` and the qio `Queue`. Run with
//! `RUSTFLAGS="--cfg loom" cargo test -p purg-core --features std --test loom --release`.
#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread;
use purg_core::kchan::{KChan, KMsg};
use purg_core::mux::Mux;
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
            let n = q.read(&mut buf).unwrap();
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
            let n = q.read(&mut buf).unwrap();
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

#[test]
fn mux_deliver_or_flush() {
    loom::model(|| {
        let mux = Arc::new(Mux::new());
        let tag = mux.begin().unwrap();
        let other = mux.clone();
        let worker = thread::spawn(move || {
            other.flush(tag);
            other.deliver(tag, b"x".to_vec())
        });
        let got = mux.wait(tag);
        let delivered = worker.join().unwrap();
        match got {
            Ok(msg) => assert!(delivered && msg == b"x"),
            Err(_) => assert!(!delivered),
        }
    });
}

#[test]
fn kchan_send_reaches_recv() {
    loom::model(|| {
        let ch = Arc::new(KChan::new(1));
        let sender = {
            let ch = ch.clone();
            thread::spawn(move || {
                ch.send(KMsg {
                    bytes: vec![7],
                    caps: Vec::new(),
                })
                .unwrap()
            })
        };
        let msg = ch.recv().unwrap().unwrap();
        sender.join().unwrap();
        assert_eq!(msg.bytes, vec![7]);
    });
}
