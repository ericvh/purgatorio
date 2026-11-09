//! In-module concurrency: Limbo's `spawn` and `chan of T` for one prog.
//!
//! A WASM instance has one thread, so tasks are futures run by a
//! cooperative executor: [`spawn`] adds a task, and [`run`] (or
//! [`block_on`]) polls ready tasks until none are left. A task that waits on
//! a [`Chan`] yields to the others. Blocking system calls block every task
//! in the prog, as they would block the whole Dis thread in Inferno; to
//! overlap them, start another prog with [`crate::spawn`].

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

type Task = Pin<Box<dyn Future<Output = ()>>>;

/// Task ids made ready by their wakers. Wakers must be `Send`, so this is
/// shared and locked even though only its own thread runs the tasks.
type Ready = Arc<Mutex<VecDeque<usize>>>;

thread_local! {
    static TASKS: RefCell<Vec<Option<Task>>> = const { RefCell::new(Vec::new()) };
    static READY: Ready = Ready::default();
}

fn push(q: &Ready, id: usize) {
    q.lock().unwrap_or_else(|e| e.into_inner()).push_back(id);
}

fn ready_push(id: usize) {
    READY.with(|q| push(q, id));
}

fn ready_pop() -> Option<usize> {
    READY.with(|q| q.lock().unwrap_or_else(|e| e.into_inner()).pop_front())
}

struct TaskWaker {
    id: usize,
    ready: Ready,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        push(&self.ready, self.id);
    }
}

/// Starts `f` as a task. It first runs at the next [`run`] or [`block_on`].
pub fn spawn(f: impl Future<Output = ()> + 'static) {
    let id = TASKS.with(|t| {
        let mut t = t.borrow_mut();
        match t.iter().position(Option::is_none) {
            Some(i) => {
                t[i] = Some(Box::pin(f));
                i
            }
            None => {
                t.push(Some(Box::pin(f)));
                t.len() - 1
            }
        }
    });
    ready_push(id);
}

/// Polls one ready task. False when no task is ready.
fn step() -> bool {
    let Some(id) = ready_pop() else {
        return false;
    };
    let Some(mut task) = TASKS.with(|t| t.borrow_mut().get_mut(id).and_then(Option::take)) else {
        return true;
    };
    let waker = Waker::from(Arc::new(TaskWaker {
        id,
        ready: READY.with(Ready::clone),
    }));
    if task
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending()
    {
        TASKS.with(|t| t.borrow_mut()[id] = Some(task));
    }
    true
}

/// Runs tasks until none is ready. Tasks still waiting on a channel that no
/// one will use again are left blocked, like Limbo threads in deadlock.
pub fn run() {
    while step() {}
}

/// Runs `f` and the other tasks until `f` completes. Returns `None` if `f`
/// can never complete because every task is blocked.
pub fn block_on<T: 'static>(f: impl Future<Output = T> + 'static) -> Option<T> {
    let out = Rc::new(RefCell::new(None));
    let slot = out.clone();
    spawn(async move {
        *slot.borrow_mut() = Some(f.await);
    });
    while out.borrow().is_none() && step() {}
    out.take()
}

/// Lets the other ready tasks run.
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

struct Inner<T> {
    queue: VecDeque<T>,
    cap: usize,
    /// Values received so far; an unbuffered sender waits for its value's
    /// sequence number to be passed.
    taken: u64,
    sent: u64,
    receivers: Vec<Waker>,
    senders: Vec<Waker>,
}

/// A channel between tasks, like Limbo's `chan of T`. With capacity 0 a send
/// completes only when a receiver has taken the value; with capacity `n` up
/// to `n` values are buffered. Clones refer to the same channel.
pub struct Chan<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Clone for Chan<T> {
    fn clone(&self) -> Self {
        Chan {
            inner: self.inner.clone(),
        }
    }
}

/// A new channel buffering `cap` values (0 for unbuffered).
pub fn chan<T>(cap: usize) -> Chan<T> {
    Chan {
        inner: Rc::new(RefCell::new(Inner {
            queue: VecDeque::new(),
            cap,
            taken: 0,
            sent: 0,
            receivers: Vec::new(),
            senders: Vec::new(),
        })),
    }
}

fn wake_all(w: &mut Vec<Waker>) {
    for w in w.drain(..) {
        w.wake();
    }
}

impl<T> Chan<T> {
    /// Sends `v`, waiting for buffer space or, if unbuffered, for a
    /// receiver.
    pub async fn send(&self, v: T) {
        let mut v = Some(v);
        let mut seq = None;
        std::future::poll_fn(|cx| {
            let mut c = self.inner.borrow_mut();
            if seq.is_none() {
                if c.queue.len() >= c.cap.max(1) {
                    c.senders.push(cx.waker().clone());
                    return Poll::Pending;
                }
                c.queue.push_back(v.take().expect("value sent once"));
                c.sent += 1;
                seq = Some(c.sent);
                wake_all(&mut c.receivers);
            }
            if c.cap == 0 && c.taken < seq.expect("set above") {
                c.senders.push(cx.waker().clone());
                return Poll::Pending;
            }
            Poll::Ready(())
        })
        .await
    }

    /// Receives the next value, waiting for one.
    pub async fn recv(&self) -> T {
        std::future::poll_fn(|cx| match self.try_recv() {
            Some(v) => Poll::Ready(v),
            None => {
                self.inner.borrow_mut().receivers.push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    /// Receives a value if one is waiting.
    pub fn try_recv(&self) -> Option<T> {
        let mut c = self.inner.borrow_mut();
        let v = c.queue.pop_front()?;
        c.taken += 1;
        wake_all(&mut c.senders);
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbuffered_send_waits_for_receiver() {
        let c = chan::<u32>(0);
        let log = Rc::new(RefCell::new(Vec::new()));
        {
            let (c, log) = (c.clone(), log.clone());
            spawn(async move {
                for i in 0..3 {
                    c.send(i).await;
                    log.borrow_mut().push(format!("sent {i}"));
                }
            });
        }
        let got = {
            let log = log.clone();
            block_on(async move {
                let mut got = Vec::new();
                for _ in 0..3 {
                    yield_now().await;
                    let v = c.recv().await;
                    log.borrow_mut().push(format!("got {v}"));
                    got.push(v);
                }
                got
            })
        };
        run();
        assert_eq!(got, Some(vec![0, 1, 2]));
        let log = log.borrow();
        // A send completes only after its value is received.
        for i in 0..3 {
            let got = log.iter().position(|l| *l == format!("got {i}")).unwrap();
            let sent = log.iter().position(|l| *l == format!("sent {i}")).unwrap();
            assert!(got < sent, "{log:?}");
        }
    }

    #[test]
    fn buffered_and_deadlock() {
        let c = chan::<&str>(2);
        let out = block_on({
            let c = c.clone();
            async move {
                c.send("a").await;
                c.send("b").await;
                (c.recv().await, c.recv().await)
            }
        });
        assert_eq!(out, Some(("a", "b")));
        assert_eq!(block_on(async move { c.recv().await }), None);
    }
}
