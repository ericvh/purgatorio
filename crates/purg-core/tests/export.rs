//! `export` serves requests concurrently (issue #69): a request blocked in
//! a file does not hold up the others, `Tflush` interrupts it, and a
//! hangup leaves no worker behind.
#![cfg(feature = "std")]

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Mock, enter, setup};
use purg_abi::mount::{MNOAUTH, MREPL};
use purg_abi::{error, mode};
use purg_core::rw::{read_msg, write_full};
use purg_core::{Chan, Prog};
use purg_platform::sync::Note;
use purg_styx::{Fcall, MAX_MSIZE, Msg, NOFID, NOTAG, VERSION};

const LONG: Duration = Duration::from_secs(20);
const SHORT: Duration = Duration::from_millis(200);

/// Runs `f` on a thread of its own and fails, instead of hanging, when it
/// does not finish.
fn within<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(f());
    });
    result
        .recv_timeout(LONG)
        .unwrap_or_else(|_| panic!("{what}: stalled"))
}

/// An export of `#R` by a child prog, on a pipe whose other end is the fd
/// returned. The tree holds a pipe at `#R/box`: reads of `box/data` block
/// until something is written to the fd in `Served::feed`.
struct Served {
    server: thread::JoinHandle<()>,
    pid: u32,
    /// The client's end of the connection.
    conn: i32,
    /// Open on `box/data1`: what is written here is read from `box/data`.
    feed: i32,
    /// Open on `box/data`, so the pipe outlives the clients' opens.
    hold: i32,
}

fn serve(p: &mut Prog, mock: &Arc<Mock>) -> Served {
    p.bind("#|", "#R/box", MREPL).unwrap();
    let feed = p.open("#R/box/data1", mode::ORDWR).unwrap();
    let hold = p.open("#R/box/data", mode::ORDWR).unwrap();
    let fd = p.create("#R/plain", mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, b"plain").unwrap(), 5);
    p.close(fd).unwrap();

    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(purg_abi::pctl::NEWFD, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let pid = child.pid;
    let (ready, started) = mpsc::channel();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        enter(&mock, &child);
        ready.send(()).unwrap();
        child.export(0, "#R", MNOAUTH).unwrap();
    });
    started.recv().unwrap();
    Served {
        server,
        pid,
        conn: b,
        feed,
        hold,
    }
}

/// Waits for every kproc the platform started to finish.
fn kprocs_gone(mock: &Mock) {
    let until = Instant::now() + LONG;
    while mock.kprocs.load(Ordering::Acquire) != 0 {
        assert!(Instant::now() < until, "kprocs left behind");
        thread::sleep(Duration::from_millis(5));
    }
}

/// A 9P client that speaks on the connection itself, so a test decides
/// what is outstanding and sees every reply.
struct Raw {
    fd: Arc<Chan>,
    replies: mpsc::Receiver<(u16, String)>,
    /// The reply reader, and the note that ends it.
    reader: Option<thread::JoinHandle<()>>,
    note: Arc<Note>,
}

/// Dropping the client hangs up: its reader lets go of the connection.
impl Drop for Raw {
    fn drop(&mut self) {
        self.note.kill();
        self.reader.take().unwrap().join().unwrap();
    }
}

impl Raw {
    fn new(p: &mut Prog, conn: i32) -> Raw {
        let fd = p.chan(conn).unwrap();
        p.close(conn).unwrap();
        let (tx, replies) = mpsc::channel();
        let note = Arc::new(Note::new());
        let (from, mine) = (fd.clone(), note.clone());
        let reader = thread::spawn(move || {
            purg_platform::sync::set_current_note(Some(mine));
            while let Ok(buf) = read_msg(&from, MAX_MSIZE) {
                let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
                if tx.send((msg.tag, format!("{:?}", msg.fcall))).is_err() {
                    break;
                }
            }
        });
        let raw = Raw {
            fd,
            replies,
            reader: Some(reader),
            note,
        };
        raw.send(
            NOTAG,
            Fcall::Tversion {
                msize: MAX_MSIZE,
                version: VERSION,
            },
        );
        let (tag, text) = raw.recv();
        assert_eq!(tag, NOTAG);
        assert!(text.starts_with("Rversion"), "{text}");
        raw.call(
            0,
            Fcall::Tattach {
                fid: 0,
                afid: NOFID,
                uname: "inferno",
                aname: "",
            },
            "Rattach",
        );
        raw
    }

    fn send(&self, tag: u16, fcall: Fcall<'_>) {
        let bytes = Msg { tag, fcall }.encode().unwrap();
        write_full(&self.fd, &bytes).unwrap();
    }

    fn recv(&self) -> (u16, String) {
        self.replies.recv_timeout(LONG).expect("no reply")
    }

    fn quiet(&self) {
        if let Ok((tag, text)) = self.replies.recv_timeout(SHORT) {
            panic!("unexpected reply: tag {tag} {text}");
        }
    }

    /// One request and its reply, which must be of kind `want`.
    fn call(&self, tag: u16, fcall: Fcall<'_>, want: &str) -> String {
        self.send(tag, fcall);
        let (got, text) = self.recv();
        assert_eq!(got, tag, "{text}");
        assert!(text.starts_with(want), "wanted {want}, got {text}");
        text
    }

    /// Makes `fid` an open fid on `box/data`.
    fn open_pipe(&self, fid: u32) {
        self.call(
            1,
            Fcall::Twalk {
                fid: 0,
                newfid: fid,
                wnames: vec!["box", "data"],
            },
            "Rwalk",
        );
        self.call(
            1,
            Fcall::Topen {
                fid,
                mode: mode::ORDWR,
            },
            "Ropen",
        );
    }

    fn read(&self, tag: u16, fid: u32) {
        self.send(
            tag,
            Fcall::Tread {
                fid,
                offset: 0,
                count: 16,
            },
        );
    }
}

/// The case in the issue, through a mount: a read blocked on an exported
/// pipe, and other requests on the same connection meanwhile.
#[test]
fn a_blocked_read_does_not_stall_the_connection() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    p.mount(s.conn, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(s.conn).unwrap();

    let fd = p.open("#R/mnt/box/data", mode::ORDWR).unwrap();
    let pipe = p.chan(fd).unwrap();
    let reader = {
        let pipe = pipe.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16];
            let n = pipe.dev.read(&pipe, &mut buf, 0).unwrap();
            buf[..n].to_vec()
        })
    };
    thread::sleep(SHORT);

    // Other files on the connection are served while the read waits.
    let m = mock.clone();
    let mut p = within("a second request", move || {
        enter(&m, &p);
        let fd = p.open("#R/mnt/plain", mode::OREAD).unwrap();
        let mut buf = [0u8; 16];
        let n = p.read(fd, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"plain");
        p.close(fd).unwrap();
        p
    });
    enter(&mock, &p);

    // So is a write on the fid the read is blocked on.
    let n = within("a write on the same fid", move || {
        pipe.dev.write(&pipe, b"up", 0).unwrap()
    });
    assert_eq!(n, 2);
    let mut buf = [0u8; 16];
    let n = p.read(s.feed, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"up");

    assert_eq!(p.write(s.feed, b"down").unwrap(), 4);
    assert_eq!(reader.join().unwrap(), b"down");

    p.close(fd).unwrap();
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
}

/// `Tflush` interrupts the request it names. The flushed request gets no
/// answer of its own, `Rflush` follows at once, and the tag can be used
/// again.
#[test]
fn flush_interrupts_a_blocked_request() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    let raw = Raw::new(&mut p, s.conn);
    raw.open_pipe(1);

    raw.read(7, 1);
    raw.quiet();
    raw.call(8, Fcall::Tflush { oldtag: 7 }, "Rflush");
    raw.quiet();

    // A flush of a tag that is not outstanding is answered as it is.
    raw.call(9, Fcall::Tflush { oldtag: 7 }, "Rflush");

    // Two flushes of one request are both answered, in order, when it ends.
    raw.read(7, 1);
    raw.quiet();
    raw.send(8, Fcall::Tflush { oldtag: 7 });
    raw.send(9, Fcall::Tflush { oldtag: 7 });
    assert_eq!(raw.recv().0, 8);
    assert_eq!(raw.recv().0, 9);
    raw.quiet();

    // The fid still works, and so does the tag.
    raw.read(7, 1);
    raw.quiet();
    assert_eq!(p.write(s.feed, b"data").unwrap(), 4);
    let (tag, text) = raw.recv();
    assert_eq!(tag, 7);
    assert!(text.starts_with("Rread"), "{text}");

    drop(raw);
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
}

/// A request that is flushed before a worker runs it still runs: a
/// `Tclunk` the client did not wait for must not leave the fid behind.
#[test]
fn a_flushed_clunk_still_clunks() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    let raw = Raw::new(&mut p, s.conn);
    for round in 0..50 {
        raw.call(
            1,
            Fcall::Twalk {
                fid: 0,
                newfid: 5,
                wnames: vec!["plain"],
            },
            "Rwalk",
        );
        raw.send(2, Fcall::Tclunk { fid: 5 });
        raw.send(3, Fcall::Tflush { oldtag: 2 });
        // Rclunk then Rflush, or Rflush alone; never Rflush first.
        let (tag, text) = raw.recv();
        if tag == 2 {
            assert!(text.starts_with("Rclunk"), "round {round}: {text}");
            assert_eq!(raw.recv().0, 3);
        } else {
            assert_eq!(tag, 3, "round {round}: {text}");
        }
        let text = raw.call(4, Fcall::Tstat { fid: 5 }, "Rerror");
        assert!(text.contains("unknown fid"), "round {round}: {text}");
    }
    drop(raw);
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
}

/// More blocked requests than idle workers: each gets a worker, the
/// connection keeps answering, and a hangup reaps them all.
#[test]
fn hangup_reaps_blocked_workers() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    let raw = Raw::new(&mut p, s.conn);
    for fid in 1..=6 {
        raw.open_pipe(fid);
        raw.read(10 + fid as u16, fid);
    }
    raw.quiet();
    raw.call(2, Fcall::Tstat { fid: 0 }, "Rstat");
    assert!(mock.kprocs.load(Ordering::Acquire) >= 6);

    drop(raw);
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
    // The fids were clunked: only the test's own open holds the pipe's
    // read end, and the pipe hangs up when that goes.
    assert_eq!(p.write(s.feed, b"x").unwrap(), 1);
    p.close(s.hold).unwrap();
    let err = p.write(s.feed, b"x").unwrap_err();
    assert_eq!(err.as_str(), error::EHUNGUP);
}

/// Killing the exporting prog ends the export, blocked requests and all.
#[test]
fn killing_the_exporter_reaps_blocked_workers() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    let raw = Raw::new(&mut p, s.conn);
    for fid in 1..=3 {
        raw.open_pipe(fid);
        raw.read(10 + fid as u16, fid);
    }
    raw.quiet();
    p.kernel.procs.get(s.pid).unwrap().kill();
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
    drop(raw);
}

/// `Tversion` starts the conversation again: what was outstanding is
/// aborted without an answer and the fids are gone.
#[test]
fn version_aborts_requests_and_clunks_fids() {
    let (mut p, mock) = setup();
    let s = serve(&mut p, &mock);
    let raw = Raw::new(&mut p, s.conn);
    raw.open_pipe(1);
    raw.read(7, 1);
    raw.quiet();
    raw.call(
        NOTAG,
        Fcall::Tversion {
            msize: MAX_MSIZE,
            version: VERSION,
        },
        "Rversion",
    );
    raw.quiet();
    let text = raw.call(1, Fcall::Tstat { fid: 1 }, "Rerror");
    assert!(text.contains("unknown fid"), "{text}");
    let text = raw.call(1, Fcall::Tstat { fid: 0 }, "Rerror");
    assert!(text.contains("unknown fid"), "{text}");

    drop(raw);
    within("the export ends", move || s.server.join().unwrap());
    kprocs_gone(&mock);
}
