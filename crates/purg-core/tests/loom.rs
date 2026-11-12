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
    model.preemption_bound = Some(model.preemption_bound.map_or(5, |n| n.min(5)));
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

/// A CPU that decides to wait for an interrupt while another CPU makes
/// the change it waits for always gets an IPI: either the waker sees it
/// in the waiting set, or it sees the waker's reset and does not wait.
/// The threshold is 1: the sleeper's own block after the change must not
/// let it wait. `pre` blocks counted before it looks put the count at or
/// past the threshold already.
fn idle_gate_model(pre: usize) {
    use loom::sync::atomic::AtomicUsize;
    use purg_platform::idle::IdleGate;
    loom::model(move || {
        let gate = Arc::new(IdleGate::new());
        for _ in 0..pre {
            gate.block();
        }
        let ready = Arc::new(AtomicBool::new(false));
        let ipi = Arc::new(AtomicUsize::new(0));
        let waker = {
            let (gate, ready, ipi) = (gate.clone(), ready.clone(), ipi.clone());
            thread::spawn(move || {
                ready.store(true, Ordering::Release);
                if gate.wake() & 1 != 0 {
                    ipi.fetch_add(1, Ordering::Release);
                }
            })
        };
        let waited = !ready.load(Ordering::Acquire) && gate.block() > 1 && gate.enter(0, 1);
        waker.join().unwrap();
        // The IPI stays pending, so a wait that starts before it arrives
        // ends when it does.
        assert!(
            !waited || ipi.load(Ordering::Acquire) > 0,
            "a CPU waited through a change with no IPI"
        );
    });
}

#[test]
fn idle_gate_wake_reaches_a_waiting_cpu() {
    idle_gate_model(1);
}

#[test]
fn idle_gate_wake_reaches_a_cpu_past_the_threshold() {
    idle_gate_model(3);
}

/// An idle CPU that finds its run queue empty and waits, while another
/// CPU queues a thread, is interrupted: the queue is under a lock, the
/// waiting set is the gate's.
#[test]
fn idle_gate_queue_push_reaches_an_idle_cpu() {
    use loom::sync::Mutex;
    use loom::sync::atomic::AtomicUsize;
    use purg_platform::idle::IdleGate;
    loom::model(|| {
        let gate = Arc::new(IdleGate::new());
        let queue = Arc::new(Mutex::new(0usize));
        let ipi = Arc::new(AtomicUsize::new(0));
        let pusher = {
            let (gate, queue, ipi) = (gate.clone(), queue.clone(), ipi.clone());
            thread::spawn(move || {
                *queue.lock().unwrap() += 1;
                if gate.waiting() & 1 != 0 {
                    ipi.fetch_add(1, Ordering::Release);
                }
            })
        };
        loop {
            gate.announce(0);
            if *queue.lock().unwrap() != 0 {
                gate.leave(0);
                break;
            }
            while ipi.load(Ordering::Acquire) == 0 {
                thread::yield_now();
            }
            gate.leave(0);
        }
        pusher.join().unwrap();
    });
}

/// A parker for the wait queue models: each thread has a permit under a
/// mutex and waits for it on a condition variable, so a wakeup that is
/// missed leaves the sleeper blocked for good, which loom reports.
struct Permits {
    me: usize,
    all: Arc<Vec<(loom::sync::Mutex<bool>, loom::sync::Condvar)>>,
}

impl Permits {
    fn new(threads: usize) -> Vec<Permits> {
        let all = Arc::new(
            (0..threads)
                .map(|_| (loom::sync::Mutex::new(false), loom::sync::Condvar::new()))
                .collect::<Vec<_>>(),
        );
        (0..threads)
            .map(|me| Permits {
                me,
                all: all.clone(),
            })
            .collect()
    }
}

impl purg_platform::waitq::Parker for Permits {
    fn current(&self) -> usize {
        self.me
    }
    fn park(&self, _deadline_ns: Option<u64>) -> bool {
        let (m, cv) = &self.all[self.me];
        let mut permit = m.lock().unwrap();
        while !*permit {
            permit = cv.wait(permit).unwrap();
        }
        *permit = false;
        false
    }
    fn unpark(&self, id: usize) {
        let (m, cv) = &self.all[id];
        *m.lock().unwrap() = true;
        cv.notify_one();
    }
}

/// A sleeper on a wait queue is never left parked by a waker that
/// changes the state and then wakes the queue, whichever comes first.
#[test]
fn waitq_wakeup_is_never_missed() {
    use purg_platform::waitq::WaitQueue;
    loom::model(|| {
        let q = Arc::new(WaitQueue::new());
        let ready = Arc::new(AtomicBool::new(false));
        let mut ps = Permits::new(2);
        let (waker_p, sleeper_p) = (ps.pop().unwrap(), ps.pop().unwrap());
        let waker = {
            let (q, ready) = (q.clone(), ready.clone());
            thread::spawn(move || {
                ready.store(true, Ordering::Release);
                q.wake(&waker_p);
            })
        };
        while !ready.load(Ordering::Acquire) {
            q.wait(&sleeper_p, &mut || ready.load(Ordering::Acquire), None);
        }
        waker.join().unwrap();
    });
}

/// Two sleepers and two changes: each waker wakes the whole queue, a
/// sleeper may be woken for the other's change (and by a permit left
/// from a wake that found it already awake), and both still finish.
#[test]
fn waitq_two_sleepers_two_wakers() {
    use loom::sync::atomic::AtomicUsize;
    use purg_platform::waitq::WaitQueue;
    // Three threads, each waking or sleeping twice: every preemption
    // multiplies the run by about six (5: 40 s, 6: 4 min); unbounded it
    // does not finish in two hours. Six is one more than pull requests run.
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(model.preemption_bound.map_or(6, |n| n.min(6)));
    model.check(|| {
        let q = Arc::new(WaitQueue::new());
        let n = Arc::new(AtomicUsize::new(0));
        let mut ps = Permits::new(3);
        let main_p = ps.pop().unwrap();
        let sleepers: Vec<_> = ps
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let (q, n) = (q.clone(), n.clone());
                thread::spawn(move || {
                    while n.load(Ordering::Acquire) <= i {
                        q.wait(&p, &mut || n.load(Ordering::Acquire) > i, None);
                    }
                })
            })
            .collect();
        for _ in 0..2 {
            n.fetch_add(1, Ordering::AcqRel);
            q.wake(&main_p);
        }
        for s in sleepers {
            s.join().unwrap();
        }
    });
}

/// A debugger and the engine holding a stopped prog (`debug::Debug::hold`):
/// a read is answered, and `resume` returns only once the prog has left
/// the stop, whatever the interleaving.
#[test]
fn debug_hold_serves_then_resumes() {
    use purg_core::debug::{Access, Debug, Layout, Resume};
    use purg_platform::sync::Note;
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(model.preemption_bound.map_or(5, |n| n.min(5)));
    model.check(|| {
        let d = Arc::new(Debug::default());
        d.set_traced(Layout::default());
        let note = Arc::new(Note::new());
        let engine = {
            let (d, note) = (d.clone(), note.clone());
            thread::spawn(move || d.hold(&note, |_| Ok(vec![7])))
        };
        while !d.stopped() {
            thread::yield_now();
        }
        assert_eq!(d.access(Access::Read { at: 0, len: 1 }).unwrap(), [7]);
        d.resume(Resume::Cont).unwrap();
        assert!(!d.stopped());
        assert_eq!(engine.join().unwrap(), Some(Resume::Cont));
    });
}

/// An interrupt posted while a prog goes to sleep is never missed: the
/// sleeper ends with the note pending, whichever runs first.
#[test]
fn note_interrupt_wakes_an_interruptible_sleeper() {
    use purg_platform::sync::Note;
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(model.preemption_bound.map_or(5, |n| n.min(5)));
    model.check(|| {
        let note = Arc::new(Note::new());
        let r = Rendez::new();
        let poster = {
            let note = note.clone();
            thread::spawn(move || note.interrupt())
        };
        assert!(!note.sleep_interruptible(&r, || false));
        assert!(note.take_interrupt());
        assert!(!note.killed());
        poster.join().unwrap();
    });
}
