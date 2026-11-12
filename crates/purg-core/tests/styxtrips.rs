//! How many requests the mount driver (`#M`) sends for common work
//! (issue #138): a deep walk is one `Twalk` for each 16 names, `Tclunk`
//! is not waited for unless the clunk can matter to a later request, and
//! a read moves up to 64 KiB.
//!
//! The client mounts an export of `#R` through a relay that logs each
//! request and can hold back every `Tclunk`.
#![cfg(feature = "std")]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use common::{Mock, enter, setup};
use purg_abi::mount::{MAFTER, MNOAUTH, MREPL};
use purg_abi::{error, mode, pctl};
use purg_core::Prog;
use purg_core::rw::{read_msg, write_full};
use purg_styx::{Fcall, IOHDRSZ, MAX_MSIZE, Msg, NOTAG, VERSION};

const LONG: Duration = Duration::from_secs(20);

/// The 17 names below `deep` of the deep file, as in `cargo xtask lethe-perf`.
const DEEP: &str = "d01/d02/d03/d04/d05/d06/d07/d08/d09/d10/d11/d12/d13/d14/d15/d16/leaf";

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

/// The requests the relay passed on, as `Twalk 16` (with the number of
/// names), `Tread 65536` (with the count), or the bare kind.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn since(&self, mark: usize) -> Vec<String> {
        self.0.lock().unwrap()[mark..].to_vec()
    }
}

fn describe(f: &Fcall<'_>) -> String {
    match f {
        Fcall::Twalk { wnames, .. } => format!("Twalk {}", wnames.len()),
        Fcall::Tread { count, .. } => format!("Tread {count}"),
        Fcall::Tversion { msize, .. } => format!("Tversion {msize}"),
        other => format!("{other:?}")
            .split([' ', '{'])
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

struct Rig {
    p: Prog,
    log: Log,
    /// Set to hold back every `Tclunk`.
    hold: Arc<AtomicBool>,
    /// Passes on the `Tclunk`s held back so far.
    release: mpsc::Sender<()>,
    mock: Arc<Mock>,
}

impl Rig {
    /// A tree in `#R` exported by a child prog, and mounted by `p` on
    /// `#R/mnt` through the relay.
    fn new() -> Rig {
        let (mut p, mock) = setup();
        let mut dir = String::from("#R/deep");
        let fd = p
            .create(&dir, mode::OREAD, purg_styx::DMDIR | 0o755)
            .unwrap();
        p.close(fd).unwrap();
        for name in DEEP.split('/') {
            dir.push('/');
            dir.push_str(name);
            let perm = if name == "leaf" {
                0o644
            } else {
                purg_styx::DMDIR | 0o755
            };
            let fd = p.create(&dir, mode::OREAD, perm).unwrap();
            p.close(fd).unwrap();
        }
        put(&mut p, "#R/one", &[b'1'; 64]);
        let big: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
        put(&mut p, "#R/big", &big);
        for d in ["#R/a", "#R/a/b", "#R/a/b/c", "#R/other", "#R/other/c"] {
            let fd = p.create(d, mode::OREAD, purg_styx::DMDIR | 0o755).unwrap();
            p.close(fd).unwrap();
        }
        put(&mut p, "#R/a/b/c/below", b"server");
        put(&mut p, "#R/other/c/bound", b"bound");

        let conn = serve(&mut p, &mock);
        let log = Log::default();
        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::new(Mutex::new(Vec::new()));
        let [near, far] = p.pipe().unwrap();
        let (to_srv, from_cli) = (p.chan(conn).unwrap(), p.chan(near).unwrap());
        p.close(conn).unwrap();
        p.close(near).unwrap();
        {
            let (from_cli, to_srv, hold, held, log) = (
                from_cli.clone(),
                to_srv.clone(),
                hold.clone(),
                held.clone(),
                log.clone(),
            );
            thread::spawn(move || {
                while let Ok(buf) = read_msg(&from_cli, MAX_MSIZE) {
                    let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
                    if matches!(msg.fcall, Fcall::Tclunk { .. }) && hold.load(Ordering::Acquire) {
                        held.lock().unwrap().push(buf);
                        continue;
                    }
                    log.0.lock().unwrap().push(describe(&msg.fcall));
                    if write_full(&to_srv, &buf).is_err() {
                        break;
                    }
                }
            });
        }
        let (release, released) = mpsc::channel::<()>();
        {
            let to_srv = to_srv.clone();
            thread::spawn(move || {
                while released.recv().is_ok() {
                    let bufs: Vec<Vec<u8>> = held.lock().unwrap().drain(..).collect();
                    for buf in bufs {
                        let _ = write_full(&to_srv, &buf);
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
        p.mount(far, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
        p.close(far).unwrap();
        Rig {
            p,
            log,
            hold,
            release,
            mock,
        }
    }

    /// What `f` sent, once everything it sent has reached the relay: a
    /// stat of the mount's root (one `Tstat`) follows it on the wire.
    fn sent(&mut self, f: impl FnOnce(&mut Prog)) -> Vec<String> {
        let mark = self.log.len();
        f(&mut self.p);
        self.p.stat("#R/mnt").unwrap();
        let mut seen = self.log.since(mark);
        assert_eq!(seen.pop().as_deref(), Some("Tstat"));
        seen
    }
}

fn put(p: &mut Prog, path: &str, data: &[u8]) {
    let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, data).unwrap(), data.len());
    p.close(fd).unwrap();
}

/// An export of `#R` by a child prog; returns the client's end.
fn serve(p: &mut Prog, mock: &Arc<Mock>) -> i32 {
    serve_at(p, mock, "#R")
}

/// An export of `root` by a child prog; returns the client's end.
fn serve_at(p: &mut Prog, mock: &Arc<Mock>, root: &'static str) -> i32 {
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(pctl::NEWFD | pctl::FORKNS, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let (ready, started) = mpsc::channel();
    let mock = mock.clone();
    thread::spawn(move || {
        enter(&mock, &child);
        ready.send(()).unwrap();
        let _ = child.export(0, root, MNOAUTH);
    });
    started.recv().unwrap();
    b
}

fn count(sent: &[String], what: &str) -> usize {
    sent.iter().filter(|s| s.as_str() == what).count()
}

/// A stat of `deep` and the 17 names below it: two walks (16 names, then
/// two), the stat, and the two fids clunked without waiting: three round
/// trips, where walking one name at a time took 39.
#[test]
fn a_deep_walk_is_one_twalk_for_16_names() {
    let mut r = Rig::new();
    let path = format!("#R/mnt/deep/{DEEP}");
    let sent = r.sent(|p| {
        let d = p.stat(&path).unwrap();
        assert_eq!(d.name, "leaf");
    });
    assert_eq!(sent, ["Twalk 16", "Twalk 2", "Tclunk", "Tstat", "Tclunk"]);
}

/// `export` walks a fid's Chan one name at a time (issue #185): a walk
/// of `deep` and the 17 names below it costs the server 18 names walked
/// below it, where resolving each name from the root walked 171: each
/// name and every name above it. The export is of the relayed mount, so
/// the relay sees what the export walks. Mount points and unions in the
/// exporter's namespace are crossed as a name resolved in full crosses
/// them.
#[test]
fn an_export_walks_each_name_once() {
    let mut r = Rig::new();
    r.p.bind("#R/other", "#R/mnt/a/b", MREPL).unwrap();
    r.p.bind("#R/deep", "#R/mnt/a", MAFTER).unwrap();
    let conn = serve_at(&mut r.p, &r.mock.clone(), "#R/mnt");
    r.p.mount(conn, -1, "#R/box", MREPL | MNOAUTH, "").unwrap();
    r.p.close(conn).unwrap();
    let mark = r.log.len();
    let d = r.p.stat(&format!("#R/box/deep/{DEEP}")).unwrap();
    assert_eq!(d.name, "leaf");
    let walked: usize = r
        .log
        .since(mark)
        .iter()
        .filter_map(|s| s.strip_prefix("Twalk "))
        .map(|n| n.parse::<usize>().unwrap())
        .sum();
    assert_eq!(walked, 18, "names walked through the relay");
    let d = r.p.stat("#R/box/a/b/c/bound").unwrap();
    assert_eq!(d.name, "bound");
    let err = r.p.stat("#R/box/a/b/c/below").unwrap_err();
    assert_eq!(err.as_str(), error::ENONEXIST);
    let d = r.p.stat("#R/box/a/d01/d02").unwrap();
    assert_eq!(d.name, "d02");
    let d = r.p.stat("#R/box/a/b/c/../../d01/../b/c/bound").unwrap();
    assert_eq!(d.name, "bound");
    let fd = r.p.open("#R/box/a/b/c/bound", mode::OREAD).unwrap();
    let mut buf = [0u8; 16];
    let n = r.p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"bound");
    r.p.close(fd).unwrap();
}

/// Open, read and close a small file: walk, open and read are waited
/// for; the clunk of a file open for reading is not.
#[test]
fn open_read_close_is_three_round_trips() {
    let mut r = Rig::new();
    let sent = r.sent(|p| {
        let fd = p.open("#R/mnt/one", mode::OREAD).unwrap();
        let mut buf = [0u8; 128];
        assert_eq!(p.read(fd, &mut buf).unwrap(), 64);
        p.close(fd).unwrap();
    });
    assert_eq!(sent, ["Twalk 1", "Topen", "Tread 128", "Tclunk"]);
}

/// A 1 MiB file read in 64 KiB reads: one `Tread` each, and one more at
/// the end of the file.
#[test]
fn a_large_read_moves_64k_a_request() {
    let mut r = Rig::new();
    let sent = r.sent(|p| {
        let fd = p.open("#R/mnt/big", mode::OREAD).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = Vec::new();
        loop {
            let n = p.read(fd, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            assert_eq!(n, buf.len());
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got.len(), 1 << 20);
        assert!(got.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8));
        p.close(fd).unwrap();
    });
    assert_eq!(count(&sent, "Tread 65536"), 17);
    assert_eq!(sent.len(), 17 + 3);
}

/// Nothing waits for the `Rclunk` of a walk's fids or of a read-only
/// file: with every `Tclunk` swallowed, the work still finishes.
#[test]
fn clunks_are_not_waited_for() {
    let r = Rig::new();
    r.hold.store(true, Ordering::Release);
    let path = format!("#R/mnt/deep/{DEEP}");
    let Rig { mut p, .. } = r;
    within("stat and read with clunks held", move || {
        p.stat(&path).unwrap();
        let fd = p.open("#R/mnt/one", mode::OREAD).unwrap();
        p.close(fd).unwrap();
        p.stat(&path).unwrap();
        // Unmounting reads the replies still due before it hangs up, and
        // these never come.
        std::mem::forget(p);
    });
}

/// A file open for writing is clunked and waited for: a server that
/// serves requests concurrently could otherwise serve a later request
/// first, and see the file still open.
#[test]
fn the_clunk_of_a_written_file_is_waited_for() {
    let mut r = Rig::new();
    let fd = r.p.open("#R/mnt/one", mode::OWRITE).unwrap();
    r.hold.store(true, Ordering::Release);
    let Rig { mut p, .. } = r;
    let (done, closed) = mpsc::channel();
    thread::spawn(move || {
        let _ = p.close(fd);
        let _ = done.send(());
    });
    assert!(closed.recv_timeout(Duration::from_millis(300)).is_err());
}

/// A name in the batch that is a mount point in the client's namespace
/// ends the walk there: the rest is walked in what is mounted, not in the
/// server's tree beneath it.
#[test]
fn a_walk_crosses_a_mount_point_inside_a_batch() {
    let mut r = Rig::new();
    r.p.bind("#R/other", "#R/mnt/a/b", MREPL).unwrap();
    let d = r.p.stat("#R/mnt/a/b/c/bound").unwrap();
    assert_eq!(d.name, "bound");
    // `below` is in the server's a/b/c, which the bind hides.
    let err = r.p.stat("#R/mnt/a/b/c/below").unwrap_err();
    assert_eq!(err.as_str(), error::ENONEXIST);
    let fd = r.p.open("#R/mnt/a/b/c/bound", mode::OREAD).unwrap();
    let mut buf = [0u8; 16];
    let n = r.p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"bound");
}

/// A walk that stops short fails as Inferno's does: `file does not exist`
/// below a directory, `not a directory` below a file.
#[test]
fn a_partial_walk_fails_as_inferno_does() {
    let mut r = Rig::new();
    let err = r.p.stat("#R/mnt/deep/d01/d02/none/d04").unwrap_err();
    assert_eq!(err.as_str(), error::ENONEXIST);
    let err = r.p.stat("#R/mnt/one/x/y").unwrap_err();
    assert_eq!(err.as_str(), error::ENOTDIR);
    let err = r.p.stat("#R/mnt/none").unwrap_err();
    assert_eq!(err.as_str(), error::ENONEXIST);
    let sent = r.sent(|p| {
        let _ = p.stat("#R/mnt/deep/d01/d02/none/d04");
    });
    // One walk, which stopped short: nothing to clunk.
    assert_eq!(sent, ["Twalk 5"]);
}

/// The export still serves a client that asks for 8 KiB messages and
/// walks one name at a time.
#[test]
fn export_serves_an_old_client() {
    let (mut p, mock) = setup();
    put(&mut p, "#R/big", &vec![7u8; 32 * 1024]);
    let conn = serve(&mut p, &mock);
    let fd = p.chan(conn).unwrap();
    p.close(conn).unwrap();
    let old = 8192 + IOHDRSZ;
    let call = |tag: u16, fcall: Fcall<'_>| -> Vec<u8> {
        write_full(&fd, &Msg { tag, fcall }.encode().unwrap()).unwrap();
        read_msg(&fd, old).unwrap()
    };
    let r = call(
        NOTAG,
        Fcall::Tversion {
            msize: old,
            version: VERSION,
        },
    );
    match Msg::decode(&r, old).unwrap().fcall {
        Fcall::Rversion { msize, .. } => assert_eq!(msize, old),
        other => panic!("{other:?}"),
    }
    let r = call(
        1,
        Fcall::Tattach {
            fid: 1,
            afid: purg_styx::NOFID,
            uname: "inferno",
            aname: "",
        },
    );
    assert!(matches!(
        Msg::decode(&r, old).unwrap().fcall,
        Fcall::Rattach { .. }
    ));
    let r = call(
        2,
        Fcall::Twalk {
            fid: 1,
            newfid: 2,
            wnames: vec!["big"],
        },
    );
    assert!(matches!(
        Msg::decode(&r, old).unwrap().fcall,
        Fcall::Rwalk { ref wqids } if wqids.len() == 1
    ));
    let r = call(3, Fcall::Topen { fid: 2, mode: 0 });
    match Msg::decode(&r, old).unwrap().fcall {
        Fcall::Ropen { iounit, .. } => assert_eq!(iounit, 8192),
        other => panic!("{other:?}"),
    }
    // A read for more than the msize allows gets what fits.
    let r = call(
        4,
        Fcall::Tread {
            fid: 2,
            offset: 0,
            count: 65536,
        },
    );
    match Msg::decode(&r, old).unwrap().fcall {
        Fcall::Rread { data } => assert_eq!(data.len(), 8192),
        other => panic!("{other:?}"),
    }
}

/// Dropping a mount waits for the replies still due; an interrupt does
/// not end that wait, and is not lost in it (issue #186): a prog that
/// catches interrupts sees it at its next blocking call.
#[test]
fn an_interrupt_survives_a_mount_being_dropped() {
    let mut r = Rig::new();
    let pid = r.p.pid;
    let fd = r.p.open(&format!("#p/{pid}/ctl"), mode::OWRITE).unwrap();
    r.p.write(fd, b"catchint on").unwrap();
    r.p.close(fd).unwrap();
    // The fid a stat walks to is clunked without waiting; its Rclunk is
    // still due when the mount goes.
    r.hold.store(true, Ordering::Release);
    r.p.stat("#R/mnt/one").unwrap();
    let (procs, note, release) = (
        r.p.kernel.procs.clone(),
        r.p.proc.note.clone(),
        r.release.clone(),
    );
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let Rig { mut p, mock, .. } = r;
        enter(&mock, &p);
        let unmount = p
            .unmount(None, "#R/mnt")
            .map_err(|e| e.as_str().to_string());
        let [rd, w] = p.pipe().unwrap();
        let mut b = [0u8; 1];
        let read = p.read(rd, &mut b).map_err(|e| e.as_str().to_string());
        p.close(rd).unwrap();
        p.close(w).unwrap();
        let _ = done.send((unmount, read));
    });
    assert!(
        finished.recv_timeout(Duration::from_millis(200)).is_err(),
        "the unmount did not wait for the Rclunk"
    );
    procs.interrupt(pid);
    assert!(
        finished.recv_timeout(Duration::from_millis(200)).is_err(),
        "the interrupt ended the wait"
    );
    assert!(note.interrupted(), "the wait took the interrupt");
    release.send(()).unwrap();
    let (unmount, read) = finished.recv_timeout(LONG).expect("the unmount stalled");
    assert_eq!(unmount, Ok(()));
    assert_eq!(read, Err(error::EINTR.to_string()));
    assert!(!note.killed());
}
