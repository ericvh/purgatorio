//! Kills and exits around file2chan (#182): a prog blocked on another
//! thread in each place a file2chan server or client waits must end
//! promptly when it is killed, and a client waiting for an answer must
//! be told when the server goes.

#![cfg(feature = "std")]

mod common;

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{Mock, enter, setup};
use purg_abi::{error, mode};
use purg_core::Prog;

/// How long a woken prog may take to return.
const PROMPT: Duration = Duration::from_secs(5);

/// A prog started from `p`, as `spawn` makes one.
fn child(p: &Prog) -> Prog {
    let mut c = p.spawn(0, &[]).unwrap();
    c.started();
    c
}

/// Runs `f` with `prog` current on a thread of its own, waits until it is
/// asleep through its note, and returns what `f` returns, once it does.
fn asleep<T: Send + 'static>(
    mock: &Arc<Mock>,
    prog: Prog,
    f: impl FnOnce(&mut Prog) -> T + Send + 'static,
) -> mpsc::Receiver<T> {
    let note = prog.proc.note.clone();
    let (tx, rx) = mpsc::channel();
    let m = mock.clone();
    thread::spawn(move || {
        let mut prog = prog;
        enter(&m, &prog);
        let _ = tx.send(f(&mut prog));
    });
    let start = Instant::now();
    while note.waiting_at().is_none() {
        assert!(start.elapsed() < PROMPT, "the prog never went to sleep");
        thread::sleep(Duration::from_millis(1));
    }
    rx
}

fn err<T>(r: purg_core::Result<T>) -> String {
    match r {
        Ok(_) => String::from("no error"),
        Err(e) => String::from(e.as_str()),
    }
}

#[test]
fn a_server_killed_in_its_alt_ends() {
    let (p, mock) = setup();
    let mut s = child(&p);
    let (rh, wh) = s.file2chan("#s", "alt").unwrap();
    let pid = s.pid;
    let done = asleep(&mock, s, move |s| err(s.wait_ops(&[(2, rh), (2, wh)], -1)));
    let at = p.kernel.procs.get(pid).unwrap().note.waiting_at().unwrap();
    assert!(at.file().ends_with("sysfile.rs"), "asleep at {at}");
    p.kernel.procs.kill(pid);
    assert_eq!(done.recv_timeout(PROMPT).unwrap(), error::EINTR);
}

#[test]
fn a_server_killed_in_its_receive_ends() {
    let (p, mock) = setup();
    let mut s = child(&p);
    let (_rh, wh) = s.file2chan("#s", "recv").unwrap();
    let pid = s.pid;
    let done = asleep(&mock, s, move |s| err(s.chan_recv(wh, 64)));
    p.kernel.procs.kill(pid);
    assert_eq!(done.recv_timeout(PROMPT).unwrap(), error::EINTR);
}

#[test]
fn a_client_killed_waiting_for_its_answer_ends() {
    let (p, mock) = setup();
    let mut s = child(&p);
    s.file2chan("#s", "slow").unwrap();
    let c = child(&p);
    let pid = c.pid;
    let done = asleep(&mock, c, |c| {
        let fd = c.open("#s/slow", mode::OREAD).unwrap();
        err(c.read(fd, &mut [0u8; 8]))
    });
    let at = p.kernel.procs.get(pid).unwrap().note.waiting_at().unwrap();
    assert!(at.file().ends_with("devsrv.rs"), "asleep at {at}");
    p.kernel.procs.kill(pid);
    assert_eq!(done.recv_timeout(PROMPT).unwrap(), error::EINTR);
    drop(s);
}

#[test]
fn a_client_killed_on_a_full_channel_ends() {
    let (mut p, mock) = setup();
    let mut s = child(&p);
    s.file2chan("#s", "full").unwrap();
    // Close events fill the write channel the server never reads.
    for _ in 0..16 {
        let fd = p.open("#s/full", mode::OREAD).unwrap();
        p.close(fd).unwrap();
    }
    let c = child(&p);
    let pid = c.pid;
    let done = asleep(&mock, c, |c| {
        let fd = c.open("#s/full", mode::OWRITE).unwrap();
        err(c.write(fd, b"x"))
    });
    p.kernel.procs.kill(pid);
    assert_eq!(done.recv_timeout(PROMPT).unwrap(), error::EINTR);
    drop(s);
}

/// A server that exits with a request it has not answered, read or not:
/// the client is told the channel hung up, and does not wait for ever.
#[test]
fn a_client_is_told_when_the_server_exits_unanswered() {
    for taken in [false, true] {
        let (p, mock) = setup();
        let mut s = child(&p);
        let (rh, _wh) = s.file2chan("#s", "gone").unwrap();
        let c = child(&p);
        let done = asleep(&mock, c, |c| {
            let fd = c.open("#s/gone", mode::OREAD).unwrap();
            err(c.read(fd, &mut [0u8; 8]))
        });
        if taken {
            enter(&mock, &s);
            assert!(s.chan_recv(rh, 64).unwrap().is_some());
            enter(&mock, &p);
        }
        drop(s);
        assert_eq!(
            done.recv_timeout(PROMPT).as_deref(),
            Ok(error::EHUNGUP),
            "request taken: {taken}"
        );
    }
}
