//! Binds from a mounted tree.
//!
//! One into a forked namespace goes away with that namespace (issue
//! #110): when the progs using it are gone, nothing holds the mount, its
//! reader ends and the connection hangs up.
//!
//! `ns` lists one as a bind of its name, not as a mount (issue #103), and
//! lists binds and mounts in the order they were made, as Inferno's
//! `/prog/n/ns` does.
#![cfg(feature = "std")]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Mock, enter, setup};
use purg_abi::mount::{MAFTER, MBEFORE, MCREATE, MNOAUTH, MREPL};
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

/// An export of `#R` by a child prog in a namespace of its own, on a pipe.
/// Returns the export's thread and the client's end of the pipe.
fn serve(p: &mut Prog, mock: &Arc<Mock>) -> (thread::JoinHandle<()>, i32) {
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let (ready, started) = mpsc::channel();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        enter(&mock, &child);
        ready.send(()).unwrap();
        child.export(0, "#R", MNOAUTH).unwrap();
    });
    started.recv().unwrap();
    (server, b)
}

trait MkdirAll {
    fn mkdir_all(&mut self, path: &str);
}

impl MkdirAll for Prog {
    fn mkdir_all(&mut self, path: &str) {
        let fd = self
            .create(path, mode::OREAD, purg_styx::DMDIR | 0o755)
            .unwrap();
        self.close(fd).unwrap();
    }
}

/// What cpuslave did: a session mounts the client's tree; a prog in it
/// forks the namespace, binds a directory of that tree before a local
/// one, runs a child and exits. Then the session exits.
fn session(fork_in_slave: bool) {
    let (mut p, mock) = setup();
    p.mkdir_all("#R/dev");
    let (server, conn) = serve(&mut p, &mock);

    let mut s = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(conn, 3)]).unwrap();
    s.started();
    p.close(conn).unwrap();
    enter(&mock, &s);
    s.mount(3, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    s.close(3).unwrap();
    let ns = s.pgrp.namespace();
    assert!(ns.contains("mount  #|/data1 #R/mnt \n"), "{ns}");

    let mut slave = s.spawn(0, &[]).unwrap();
    slave.started();
    enter(&mock, &slave);
    if fork_in_slave {
        slave.pctl(pctl::FORKNS, &[]).unwrap();
    }
    slave.bind("#R/mnt/dev", "#R/box", MBEFORE).unwrap();
    let ns = slave.pgrp.namespace();
    assert!(ns.contains("bind -b #R/mnt/dev #R/box\n"), "{ns}");
    let mut child = slave.spawn(0, &[]).unwrap();
    child.started();
    enter(&mock, &child);
    child.stat("#R/box").unwrap();
    drop(child);
    enter(&mock, &slave);
    drop(slave);
    enter(&mock, &s);
    drop(s);
    enter(&mock, &p);

    // Nothing refers to the mount: the connection hangs up and the export
    // ends.
    within("the export ends", move || server.join().unwrap());
    kprocs_gone(&mock);
}

#[test]
fn a_bind_from_a_mount_in_a_forked_namespace_is_released() {
    session(true);
}

#[test]
fn a_bind_from_a_mount_in_the_same_namespace_is_released() {
    session(false);
}

/// A prog lets go of its namespace while it can still be killed (issue
/// #110). Releasing a bind from a mounted tree clunks a fid, and the file
/// server may never answer: in cpuslave's case the relay carrying the
/// conversation stops once the session's status is out. If the prog had
/// already left the prog table, nothing could interrupt that clunk, and
/// its thread would outlive every prog.
#[test]
fn an_exiting_prog_can_be_killed_while_it_releases_its_mounts() {
    let (mut p, mock) = setup();
    p.mkdir_all("#R/dev");
    let (_server, conn) = serve(&mut p, &mock);

    // A relay between the mount and the export that swallows every
    // `Tclunk` once `hold` is set.
    let [near, far] = p.pipe().unwrap();
    let (to_srv, from_cli) = (p.chan(conn).unwrap(), p.chan(near).unwrap());
    p.close(conn).unwrap();
    p.close(near).unwrap();
    let hold = Arc::new(AtomicBool::new(false));
    let (seen, clunked) = mpsc::channel();
    {
        let (from_cli, to_srv, hold) = (from_cli.clone(), to_srv.clone(), hold.clone());
        thread::spawn(move || {
            while let Ok(buf) = read_msg(&from_cli, MAX_MSIZE) {
                let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
                if matches!(msg.fcall, Fcall::Tclunk { .. }) && hold.load(Ordering::Acquire) {
                    let _ = seen.send(());
                    continue;
                }
                if write_full(&to_srv, &buf).is_err() {
                    break;
                }
            }
        });
    }
    thread::spawn(move || {
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
    assert!(
        p.kernel.procs.get(pid).is_some(),
        "the prog left the table before releasing its namespace"
    );
    p.kernel.procs.kill(pid);
    within("the killed prog exits", move || exit.join().unwrap());
    assert!(p.kernel.procs.get(pid).is_none());
}

/// What `ns` shows for a mount, binds from inside it, a bind of the mount
/// point, unions and `-c`, in the order they were made; a forked
/// namespace shows the same.
#[test]
fn ns_tells_binds_from_mounts() {
    let (mut p, mock) = setup();
    p.mkdir_all("#R/u");
    p.mkdir_all("#R/n");
    p.mkdir_all("#R/dev");
    p.mkdir_all("#R/dev/sub");
    let (server, conn) = serve(&mut p, &mock);
    let mut s = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(conn, 3)]).unwrap();
    s.started();
    p.close(conn).unwrap();
    enter(&mock, &s);

    s.mount(3, -1, "#R/mnt", MREPL | MNOAUTH | MCREATE, "dev")
        .unwrap();
    s.close(3).unwrap();
    s.bind("#R/mnt/sub", "#R/u", MREPL).unwrap();
    s.bind("#R/u", "#R/n", MAFTER).unwrap();
    s.bind("#R/mnt", "#R/box", MBEFORE | MCREATE).unwrap();
    // Binding a union binds its other members after the first, by the
    // names they were bound by, as Inferno's cmount copies them.
    s.bind("#R/n", "#R/u", MBEFORE).unwrap();
    let want = "mount -c #|/data1 #R/mnt dev\n\
                bind  #R/mnt/sub #R/u\n\
                bind -a #R/u #R/n\n\
                bind -bc #R/mnt #R/box\n\
                bind -b #R/n #R/u\n\
                bind -b #R/u #R/u\n\
                cd /\n";
    let ns = s.pgrp.namespace();
    assert_eq!(ns, want);
    s.pctl(pctl::FORKNS, &[]).unwrap();
    assert_eq!(s.pgrp.namespace(), ns);

    drop(s);
    enter(&mock, &p);
    within("the export ends", move || server.join().unwrap());
    kprocs_gone(&mock);
}
