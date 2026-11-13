//! `purgatorio.wait` on a file a Styx server serves (`#M`): the mount
//! driver sends a read ahead when asked to watch the file, the file is
//! ready once the reply has come, the next read takes the reply (and a
//! short read leaves the rest for the one after), and closing the file
//! flushes a read still outstanding.
#![cfg(feature = "std")]

mod common;

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use common::{enter, setup};
use purg_abi::mode;
use purg_abi::mount::{MNOAUTH, MREPL};
use purg_core::chan::Chan;
use purg_core::rw::{read_msg, write_full};
use purg_styx::{DMDIR, Fcall, MAX_MSIZE, Msg, QTDIR, QTFILE, Qid};

const LONG: Duration = Duration::from_secs(20);

fn qid(path: u64, dir: bool) -> Qid {
    Qid {
        path,
        vers: 0,
        qtype: if dir { QTDIR } else { QTFILE },
    }
}

fn send(c: &Chan, tag: u16, fcall: Fcall<'_>) {
    write_full(c, &Msg { tag, fcall }.encode().unwrap()).unwrap();
}

/// A server with one file, `ev`, whose reads are held until the test
/// answers them. Reports each request's kind, and the tag of each read.
fn server(c: Arc<Chan>, out: Arc<Mutex<Chan2>>, log: mpsc::Sender<(String, u16)>) {
    thread::spawn(move || {
        while let Ok(buf) = read_msg(&c, MAX_MSIZE) {
            let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
            let tag = msg.tag;
            let kind = format!("{:?}", msg.fcall)
                .split([' ', '{'])
                .next()
                .unwrap_or_default()
                .to_string();
            let o = out.lock().unwrap();
            match msg.fcall {
                Fcall::Tversion { msize, .. } => send(
                    &o.0,
                    tag,
                    Fcall::Rversion {
                        msize,
                        version: "9P2000",
                    },
                ),
                Fcall::Tattach { .. } => send(&o.0, tag, Fcall::Rattach { qid: qid(0, true) }),
                Fcall::Twalk { wnames, .. } => {
                    let wqids: Vec<Qid> = wnames.iter().map(|_| qid(1, false)).collect();
                    send(&o.0, tag, Fcall::Rwalk { wqids })
                }
                Fcall::Topen { .. } => send(
                    &o.0,
                    tag,
                    Fcall::Ropen {
                        qid: qid(1, false),
                        iounit: 0,
                    },
                ),
                Fcall::Tread { .. } => {}
                Fcall::Tflush { .. } => send(&o.0, tag, Fcall::Rflush),
                Fcall::Tclunk { .. } => send(&o.0, tag, Fcall::Rclunk),
                _ => send(&o.0, tag, Fcall::Rerror { ename: "no" }),
            }
            drop(o);
            if log.send((kind, tag)).is_err() {
                break;
            }
        }
    });
}

/// The server's end, shared with the test, which answers the reads.
struct Chan2(Arc<Chan>);

/// The next request the server saw, skipping `Tclunk`s of walks.
fn next(log: &mpsc::Receiver<(String, u16)>) -> (String, u16) {
    log.recv_timeout(LONG).expect("no request")
}

#[test]
fn a_served_file_is_ready_when_its_read_ahead_is_answered() {
    let (mut p, mock) = setup();
    enter(&mock, &p);
    let _ = p.create("#R/mnt", mode::OREAD, DMDIR | 0o755);
    let [near, far] = p.pipe().unwrap();
    let srv = p.chan(near).unwrap();
    p.close(near).unwrap();
    let out = Arc::new(Mutex::new(Chan2(srv.clone())));
    let (tx, log) = mpsc::channel();
    server(srv, out.clone(), tx);
    p.mount(far, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(far).unwrap();
    let fd = p.open("#R/mnt/ev", mode::OREAD).unwrap();
    while let Ok(r) = log.try_recv() {
        drop(r);
    }

    // Not ready: the read ahead is waiting at the server.
    assert_eq!(p.wait_ops(&[(0, fd)], 0).unwrap(), None);
    let (kind, tag) = next(&log);
    assert_eq!(kind, "Tread");
    // A second wait sends no second read.
    assert_eq!(p.wait_ops(&[(0, fd)], 0).unwrap(), None);
    send(&out.lock().unwrap().0, tag, Fcall::Rread { data: b"hello" });
    assert_eq!(p.wait_ops(&[(0, fd)], 10_000).unwrap(), Some(0));
    let mut b = [0u8; 3];
    assert_eq!(p.read(fd, &mut b).unwrap(), 3);
    assert_eq!(&b, b"hel");
    // The rest is ready, and read without asking the server.
    assert_eq!(p.wait_ops(&[(0, fd)], 0).unwrap(), Some(0));
    assert_eq!(p.read(fd, &mut b).unwrap(), 2);
    assert_eq!(&b[..2], b"lo");
    assert!(log.try_recv().is_err(), "a request for data already read");

    // A read outstanding when the file is closed is flushed.
    assert_eq!(p.wait_ops(&[(0, fd)], 0).unwrap(), None);
    let (kind, tag) = next(&log);
    assert_eq!(kind, "Tread");
    p.close(fd).unwrap();
    let (kind, _) = next(&log);
    assert_eq!(kind, "Tflush");
    let (kind, _) = next(&log);
    assert_eq!(kind, "Tclunk");
    // The late reply is dropped, and the mount still works.
    send(&out.lock().unwrap().0, tag, Fcall::Rread { data: b"late" });
    let fd = p.open("#R/mnt/ev", mode::OREAD).unwrap();
    p.close(fd).unwrap();
}
