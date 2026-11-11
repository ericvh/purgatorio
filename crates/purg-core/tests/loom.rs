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

/// A reply racing the caller giving up on it: either way the tag is
/// free afterwards.
#[test]
fn mux_discard_or_deliver() {
    loom::model(|| {
        let mux = Arc::new(Mux::new());
        let tag = mux.begin().unwrap();
        let other = mux.clone();
        let reader = thread::spawn(move || {
            other.deliver(tag, b"x".to_vec());
        });
        mux.discard(tag);
        reader.join().unwrap();
        assert_eq!(mux.outstanding(), 0);
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

/// Replies queued from several threads are each written once, and never
/// by two threads at a time, whoever ends up draining.
#[test]
fn outq_writes_each_item_once() {
    use purg_core::workq::OutQ;
    loom::model(|| {
        let q = Arc::new(OutQ::new());
        let written = Arc::new(loom::sync::Mutex::new(Vec::new()));
        let writing = Arc::new(AtomicBool::new(false));
        let send = {
            let (q, written, writing) = (q.clone(), written.clone(), writing.clone());
            move |v: u8| {
                q.push(v);
                q.drain(|x| {
                    assert!(!writing.swap(true, Ordering::AcqRel), "two writers");
                    written.lock().unwrap().push(x);
                    writing.store(false, Ordering::Release);
                });
            }
        };
        let others: Vec<_> = [1u8, 2]
            .into_iter()
            .map(|v| {
                let send = send.clone();
                thread::spawn(move || send(v))
            })
            .collect();
        send(3);
        for t in others {
            t.join().unwrap();
        }
        let mut got = written.lock().unwrap().clone();
        got.sort_unstable();
        assert_eq!(got, [1, 2, 3]);
    });
}

/// An item put while the only worker is blocked gets a worker of its own:
/// here the first item cannot finish until the second has run. Then the
/// pool closes and every worker is told to exit.
#[test]
fn workq_gives_a_blocked_item_company() {
    use purg_core::workq::{Put, WorkQ};
    // Three threads and four sleep/wakeup pairs: with five preemptions the
    // model runs for a quarter of an hour, with three for seconds.
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(model.preemption_bound.map_or(3, |n| n.min(3)));
    model.check(|| {
        let q = Arc::new(WorkQ::new(2, 1));
        let gate = Arc::new(Rendez::new());
        let open = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(loom::sync::atomic::AtomicUsize::new(0));
        let all = Arc::new(Rendez::new());
        let start = || {
            let (q, gate, open) = (q.clone(), gate.clone(), open.clone());
            let (ran, all) = (ran.clone(), all.clone());
            thread::spawn(move || {
                while let Some(item) = q.take() {
                    if item == 1 {
                        gate.sleep(|| open.load(Ordering::Acquire));
                    } else {
                        open.store(true, Ordering::Release);
                        gate.wakeup();
                    }
                    ran.fetch_add(1, Ordering::AcqRel);
                    all.wakeup();
                }
            })
        };
        let mut workers = Vec::new();
        for item in [1u8, 2] {
            if q.put(item) == Put::Start {
                workers.push(start());
            }
        }
        all.sleep(|| ran.load(Ordering::Acquire) == 2);
        assert!(q.close().is_empty());
        q.join();
        assert_eq!(q.workers(), 0);
        for w in workers {
            w.join().unwrap();
        }
    });
}

/// A worker that went idle is woken by the next item; none is started
/// for it and the wakeup is not missed.
#[test]
fn workq_wakes_an_idle_worker() {
    use purg_core::workq::{Put, WorkQ};
    loom::model(|| {
        let q = Arc::new(WorkQ::new(1, 1));
        let ran = Arc::new(loom::sync::atomic::AtomicUsize::new(0));
        let all = Arc::new(Rendez::new());
        assert_eq!(q.put(1u8), Put::Start);
        let worker = {
            let (q, ran, all) = (q.clone(), ran.clone(), all.clone());
            thread::spawn(move || {
                while q.take().is_some() {
                    ran.fetch_add(1, Ordering::AcqRel);
                    all.wakeup();
                }
            })
        };
        // The pool is full: this one waits for the worker.
        assert_eq!(q.put(2), Put::Queued);
        all.sleep(|| ran.load(Ordering::Acquire) == 2);
        q.close();
        q.join();
        worker.join().unwrap();
    });
}
