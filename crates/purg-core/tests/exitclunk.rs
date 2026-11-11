//! How a prog lets go of its files and mounts when it exits (issue #118).
//!
//! A prog killed while it waits for a clunk exits at once, and does not
//! flush the clunk: the request stays out, and the mount's reader drops
//! the reply when it comes. Its fds close when it exits, even while
//! something still holds its `Proc`.
#![cfg(feature = "std")]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Mock, enter, setup};
use purg_abi::mount::{MBEFORE, MNOAUTH, MREPL};
use purg_abi::{mode, pctl};
use purg_core::Prog;
use purg_core::rw::{read_msg, write_full};
use purg_styx::{Fcall, MAX_MSIZE, Msg};

const LONG: Duration = Duration::from_secs(20);

/// Waits for `f` on a thread of its own, failing instead of hanging.
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(f());
    });
    result
        .recv_timeout(LONG)
        .unwrap_or_else(|_| panic!("{what}: stalled"))
}

fn kprocs_gone(mock: &Mock) {
    let until = Instant::now() + LONG;
    while mock.kprocs.load(Ordering::Acquire) != 0 {
        assert!(Instant::now() < until, "kprocs left behind");
        thread::sleep(Duration::from_millis(5));
    }
}

fn mkdir(p: &mut Prog, path: &str) {
    let fd = p
        .create(path, mode::OREAD, purg_styx::DMDIR | 0o755)
        .unwrap();
    p.close(fd).unwrap();
}

/// An export of `#R` by a child prog in a namespace of its own, on a pipe.
/// Returns the export's thread, the client's end of the pipe and the
/// exporting prog's pid. The export ends when the conversation hangs up
/// or the prog is killed.
fn serve(p: &mut Prog, mock: &Arc<Mock>) -> (thread::JoinHandle<()>, i32, u32) {
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let pid = child.pid;
    let (ready, started) = mpsc::channel();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        enter(&mock, &child);
        ready.send(()).unwrap();
        let _ = child.export(0, "#R", MNOAUTH);
    });
    started.recv().unwrap();
    (server, b, pid)
}

/// How the relay in [`clunk_of_a_killed_prog`] treats the clunks it holds.
#[derive(Clone, Copy, PartialEq)]
enum Server {
    /// Answers once the prog has exited.
    Late,
    /// Never answers, and the connection then hangs up.
    Never,
}

/// A prog is killed while it waits for a clunk as it exits: it must exit
/// at once, without flushing the clunk, which stays out for the server to
/// act on. Once nothing refers to the mount it goes, reader and all.
fn clunk_of_a_killed_prog(server: Server) {
    let (mut p, mock) = setup();
    mkdir(&mut p, "#R/dev");
    let (export, conn, exporter) = serve(&mut p, &mock);

    // A relay between the mount and the export that holds back every
    // `Tclunk` once `hold` is set, and notes any `Tflush`.
    let [near, far] = p.pipe().unwrap();
    let (to_srv, from_cli) = (p.chan(conn).unwrap(), p.chan(near).unwrap());
    p.close(conn).unwrap();
    p.close(near).unwrap();
    let hold = Arc::new(AtomicBool::new(false));
    let flushed = Arc::new(AtomicBool::new(false));
    let (seen, clunked) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let client_side = {
        let (from_cli, to_srv) = (from_cli.clone(), to_srv.clone());
        let (hold, flushed) = (hold.clone(), flushed.clone());
        thread::spawn(move || {
            let mut held = 0;
            let mut late = false;
            while let Ok(buf) = read_msg(&from_cli, MAX_MSIZE) {
                let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
                match msg.fcall {
                    Fcall::Tflush { .. } => flushed.store(true, Ordering::Release),
                    Fcall::Tclunk { .. } if hold.load(Ordering::Acquire) => {
                        let _ = seen.send(());
                        held += 1;
                        // Hold the first until the prog has gone.
                        if server == Server::Late && !late {
                            let _ = released.recv();
                            late = true;
                        }
                    }
                    _ => {}
                }
                if server == Server::Never && held > 0 {
                    continue;
                }
                if write_full(&to_srv, &buf).is_err() {
                    break;
                }
            }
            // The mount hung up: so does the relay, to the export.
        })
    };
    let server_side = thread::spawn(move || {
        while let Ok(buf) = read_msg(&to_srv, MAX_MSIZE) {
            if write_full(&from_cli, &buf).is_err() {
                break;
            }
        }
    });

    let mut s = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(far, 3)]).unwrap();
    s.started();
    p.close(far).unwrap();
    enter(&mock, &s);
    s.mount(3, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    s.close(3).unwrap();
    s.bind("#R/mnt/dev", "#R/box", MBEFORE).unwrap();
    let pid = s.pid;
    hold.store(true, Ordering::Release);
    let exit = {
        let mock = mock.clone();
        thread::spawn(move || {
            enter(&mock, &s);
            drop(s);
        })
    };
    enter(&mock, &p);
    clunked
        .recv_timeout(LONG)
        .expect("the exiting prog clunks its fids");
    p.kernel.procs.kill(pid);
    within("the killed prog exits", move || exit.join().unwrap());
    assert!(p.kernel.procs.get(pid).is_none());
    let _ = release.send(());

    // Nothing refers to the mount any more: it hangs up, the relay hangs
    // up on the export, and the export ends.
    within("the relay sees the hangup", move || {
        client_side.join().unwrap();
    });
    // The export still has the relay's other side; end it.
    p.kernel.procs.kill(exporter);
    drop(p);
    within("the export ends", move || {
        export.join().unwrap();
        server_side.join().unwrap();
    });
    kprocs_gone(&mock);
    assert!(
        !flushed.load(Ordering::Acquire),
        "a killed prog flushed its clunk"
    );
}

#[test]
fn a_killed_progs_clunk_stays_out_for_a_late_server() {
    clunk_of_a_killed_prog(Server::Late);
}

#[test]
fn a_killed_progs_clunk_to_a_silent_server_leaves_no_kproc() {
    clunk_of_a_killed_prog(Server::Never);
}

/// A prog's files are closed when it exits, even while something else
/// (here the test, as an open `#p` file or another kproc's syscall would)
/// still holds its `Proc`: a reader of a pipe it wrote sees end of file.
#[test]
fn an_exited_progs_files_close_while_its_proc_is_held() {
    let (mut p, _) = setup();
    let [r, w] = p.pipe().unwrap();
    let mut child = p.spawn(pctl::NEWFD, &[(w, 1)]).unwrap();
    child.started();
    p.close(w).unwrap();
    child.write(1, b"bye").unwrap();
    let held = p.kernel.procs.get(child.pid).unwrap();
    drop(child);
    let mut buf = [0u8; 8];
    assert_eq!(p.read(r, &mut buf).unwrap(), 3);
    let reader = p.chan(r).unwrap();
    // Before the fix the writer stayed open, and this read never ended.
    let n = within("end of file on the pipe", move || {
        reader.dev.read(&reader, &mut [0u8; 8], 0)
    });
    assert_eq!(n.unwrap(), 0);
    assert!(held.fgrp().get(1).is_err(), "the Proc keeps the fd table");
}
