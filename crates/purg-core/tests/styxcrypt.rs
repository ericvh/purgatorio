//! The Styx conversation after a version 3 mount handshake is encrypted
//! and authenticated (issue #74, `docs/auth.md`). A relay between the
//! mount and the export records both directions and can change a byte on
//! the way, as a man in the middle would.
#![cfg(feature = "std")]

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use common::{Mock, enter, setup};
use purg_abi::mode;
use purg_abi::mount::{MCREATE, MNOAUTH, MREPL};
use purg_core::Prog;

const LONG: Duration = Duration::from_secs(20);
const NAME: &[u8] = b"wire-secret-name";
const DATA: &[u8] = b"a plaintext payload nobody on the wire may read";

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

fn kprocs_gone(mock: &Mock) {
    let until = Instant::now() + LONG;
    while mock.kprocs.load(Ordering::Acquire) != 0 {
        assert!(Instant::now() < until, "kprocs left behind");
        thread::sleep(Duration::from_millis(5));
    }
}

/// The public test key, trusted by the exporter and held by the client.
fn install_fixture(p: &mut Prog) {
    p.bind("#R", "/", purg_abi::mount::MAFTER | MCREATE)
        .unwrap();
    for dir in ["/lib", "/lib/test"] {
        let fd = p.create(dir, mode::OREAD, purg_styx::DMDIR).unwrap();
        p.close(fd).unwrap();
    }
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let public = purg_core::auth::public_key(&seed);
    for (path, bytes) in [
        ("/lib/test/auth.key", seed.as_slice()),
        ("/lib/test/auth.key.pub", public.as_slice()),
    ] {
        let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
        assert_eq!(p.write(fd, bytes).unwrap(), bytes.len());
        p.close(fd).unwrap();
    }
}

/// An export of `#R/box` by a child prog; returns its end of the pipe and
/// what `export` returned.
fn spawn_export(
    p: &mut Prog,
    mock: &Arc<Mock>,
    flags: u32,
) -> (thread::JoinHandle<Result<(), String>>, i32) {
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(purg_abi::pctl::NEWFD, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let (ready, started) = mpsc::channel();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        enter(&mock, &child);
        ready.send(()).unwrap();
        child
            .export(0, "#R/box", flags)
            .map_err(|e| String::from(e.as_str()))
    });
    started.recv().unwrap();
    (server, b)
}

/// Where the relay changes a byte: the last byte of frame `frame` (a 9P
/// message or a record, both of which start with their size) in one
/// direction.
#[derive(Clone, Copy)]
struct Flip {
    up: bool,
    frame: usize,
}

/// A relay between a mount and an export, one prog per direction. Each
/// copies what it reads to the other side and records it. When either
/// side ends it kills the other, so the export sees the hangup.
struct Relay {
    /// What the client sent, and what the server sent.
    up: Arc<Mutex<Vec<u8>>>,
    down: Arc<Mutex<Vec<u8>>>,
    pumps: Vec<thread::JoinHandle<()>>,
}

impl Relay {
    /// Puts a relay in front of `server`, the export's pipe end, and
    /// returns it with the end the client mounts.
    fn new(p: &mut Prog, mock: &Arc<Mock>, server: i32, flip: Option<Flip>) -> (Relay, i32) {
        let [c, r] = p.pipe().unwrap();
        let mut up = p
            .spawn(purg_abi::pctl::NEWFD, &[(r, 0), (server, 1)])
            .unwrap();
        let mut down = p
            .spawn(purg_abi::pctl::NEWFD, &[(server, 0), (r, 1)])
            .unwrap();
        up.started();
        down.started();
        p.close(r).unwrap();
        p.close(server).unwrap();
        let (up_pid, down_pid) = (up.pid, down.pid);
        let logs = [
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        ];
        let mut pumps = Vec::new();
        for (prog, log, other, is_up) in [
            (up, logs[0].clone(), down_pid, true),
            (down, logs[1].clone(), up_pid, false),
        ] {
            let mock = mock.clone();
            let flip = flip.filter(|f| f.up == is_up).map(|f| f.frame);
            pumps.push(thread::spawn(move || {
                let mut prog = prog;
                enter(&mock, &prog);
                pump(&mut prog, &log, flip);
                if let Some(o) = prog.kernel.procs.get(other) {
                    o.kill();
                }
            }));
        }
        let [up, down] = logs;
        (Relay { up, down, pumps }, c)
    }

    fn join(self) -> (Vec<u8>, Vec<u8>) {
        for t in self.pumps {
            within("the relay ends", move || t.join().unwrap());
        }
        let up = self.up.lock().unwrap().clone();
        let down = self.down.lock().unwrap().clone();
        (up, down)
    }
}

/// Copies fd 0 to fd 1 until either fails, recording what passes.
fn pump(p: &mut Prog, log: &Mutex<Vec<u8>>, flip: Option<usize>) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match p.read(0, &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        {
            let mut l = log.lock().unwrap();
            let start = l.len();
            l.extend_from_slice(&buf[..n]);
            if let Some(end) = flip.and_then(|k| frame_end(&l, k))
                && (start..start + n).contains(&(end - 1))
            {
                buf[end - 1 - start] ^= 0x01;
            }
        }
        if p.write(1, &buf[..n]).is_err() {
            return;
        }
    }
}

/// Where frame `k` of `log` ends, once its header is in.
fn frame_end(log: &[u8], k: usize) -> Option<usize> {
    let mut pos = 0;
    for i in 0..=k {
        let size = u32::from_le_bytes(log.get(pos..pos + 4)?.try_into().ok()?) as usize;
        if i == k {
            return Some(pos + size);
        }
        pos += size;
    }
    None
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Writes and reads back a file through the mount at `#R/mnt`.
fn use_mount(p: &mut Prog) {
    let name = format!("#R/mnt/{}", std::str::from_utf8(NAME).unwrap());
    let fd = p.create(&name, mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, DATA).unwrap(), DATA.len());
    p.close(fd).unwrap();
    let fd = p.open(&name, mode::OREAD).unwrap();
    let mut buf = [0u8; 128];
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], DATA);
    p.close(fd).unwrap();
}

/// After an authenticated attach nothing on the wire is plaintext Styx:
/// neither the file's name nor its contents pass in the clear, in either
/// direction. The handshake itself (the user name in `Tauth`) does.
#[test]
fn an_authenticated_mount_is_encrypted() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let (relay, c) = Relay::new(&mut p, &mock, b, None);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    let (up, down) = relay.join();
    within("the export ends", move || server.join().unwrap()).unwrap();
    assert!(contains(&up, b"inferno"), "Tauth is in the clear");
    for (dir, log) in [("up", &up), ("down", &down)] {
        assert!(!contains(log, NAME), "{dir}: the name went in the clear");
        assert!(!contains(log, DATA), "{dir}: the data went in the clear");
    }
    // The records follow the four messages of the handshake.
    assert!(frame_end(&up, 6).is_some() && frame_end(&down, 6).is_some());
    kprocs_gone(&mock);
}

/// The same through `fauth`: the caller reads the offer with the kernel's
/// ephemeral key, signs it and writes the proof; the ephemeral secret
/// stays in the kernel, which encrypts from the proof on. A caller that
/// reads only the challenge and writes a version 1 proof stays clear.
#[test]
fn fauth_with_a_version_3_proof_is_encrypted() {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    for v3 in [true, false] {
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, b) = spawn_export(&mut p, &mock, 0);
        let (relay, c) = Relay::new(&mut p, &mock, b, None);
        let afd = p.fauth(c, "").unwrap();
        let proof = if v3 {
            let mut signed = [0u8; purg_core::auth::SIGNED];
            assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
            purg_auth::proof::make(&seed, None, &signed, "inferno", "")
        } else {
            let mut challenge = [0u8; purg_core::auth::CHALLENGE];
            assert_eq!(p.read(afd, &mut challenge).unwrap(), challenge.len());
            let mut proof = purg_core::auth::sign(&seed, &challenge).to_vec();
            proof.extend_from_slice(&purg_core::auth::public_key(&seed));
            proof
        };
        assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
        p.mount(c, afd, "#R/mnt", MREPL | MCREATE, "").unwrap();
        p.close(afd).unwrap();
        p.close(c).unwrap();
        use_mount(&mut p);
        p.unmount(None, "#R/mnt").unwrap();
        let (up, down) = relay.join();
        within("the export ends", move || server.join().unwrap()).unwrap();
        assert_eq!(contains(&up, NAME), !v3);
        assert_eq!(contains(&down, DATA), !v3);
        kprocs_gone(&mock);
    }
}

/// `-A` (no `Tauth`, against an export that does not authenticate) stays
/// in the clear.
#[test]
fn an_unauthenticated_mount_stays_clear() {
    let (mut p, mock) = setup();
    let (server, b) = spawn_export(&mut p, &mock, MNOAUTH);
    let (relay, c) = Relay::new(&mut p, &mock, b, None);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE | MNOAUTH, "")
        .unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    let (up, down) = relay.join();
    within("the export ends", move || server.join().unwrap()).unwrap();
    assert!(contains(&up, NAME) && contains(&up, DATA));
    assert!(contains(&down, DATA));
    kprocs_gone(&mock);
}

/// A server that answers "authentication not required" is spoken to in
/// the clear too.
#[test]
fn a_server_without_keys_stays_clear() {
    let (mut p, mock) = setup();
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let (relay, c) = Relay::new(&mut p, &mock, b, None);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    let (up, _) = relay.join();
    within("the export ends", move || server.join().unwrap()).unwrap();
    assert!(contains(&up, NAME) && contains(&up, DATA));
    kprocs_gone(&mock);
}

/// Mounts through a relay that changes one byte and returns the mount's
/// error and the export's result. Nothing may hang.
fn tampered(flip: Flip) -> (String, Result<(), String>) {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let (relay, c) = Relay::new(&mut p, &mock, b, Some(flip));
    let m = mock.clone();
    let (p, err) = within("the mount fails", move || {
        enter(&m, &p);
        let r = p.mount(c, -1, "#R/mnt", MREPL, "");
        p.close(c).unwrap();
        (p, r.expect_err("a tampered mount succeeded"))
    });
    relay.join();
    let served = within("the export ends", move || server.join().unwrap());
    drop(p);
    kprocs_gone(&mock);
    (String::from(err.as_str()), served)
}

/// The frames of the handshake, in each direction: Tversion, Tauth, Tread
/// (the offer), Twrite (the proof), then records from the Tattach on; and
/// Rversion, Rauth, Rread, Rwrite, then records from the Rattach on.
#[test]
fn a_changed_reply_record_fails_the_mount() {
    let (err, served) = tampered(Flip {
        up: false,
        frame: 4,
    });
    assert_eq!(err, purg_auth::record::BAD_MAC);
    assert_eq!(served, Ok(()));
}

#[test]
fn a_changed_request_record_fails_the_mount() {
    let (err, served) = tampered(Flip { up: true, frame: 4 });
    // The export gives up on the connection, and the client sees it hang up.
    assert_eq!(err, purg_abi::error::EHUNGUP);
    assert_eq!(served, Err(String::from(purg_auth::record::BAD_MAC)));
}

/// A server key changed on the way makes the proof fail: the signature
/// covers it.
#[test]
fn a_changed_offer_fails_the_proof() {
    let (err, served) = tampered(Flip {
        up: false,
        frame: 2,
    });
    assert_eq!(err, purg_core::auth::FAILED);
    assert_eq!(served, Ok(()));
}

/// Stripping the server's key from the offer does not get the client to
/// speak in the clear.
#[test]
fn a_stripped_offer_is_refused() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let [c, r] = p.pipe().unwrap();
    let conn = p.chan(c).unwrap();
    let mut mitm = p.spawn(purg_abi::pctl::NEWFD, &[(r, 0), (b, 1)]).unwrap();
    mitm.started();
    p.close(r).unwrap();
    p.close(b).unwrap();
    let m = mock.clone();
    // A man in the middle that answers Tversion and Tauth by relaying, and
    // the Tread of the offer with only the challenge.
    let relay = thread::spawn(move || {
        use purg_core::rw::{read_msg, write_full};
        use purg_styx::{Fcall, MAX_MSIZE, Msg};
        enter(&m, &mitm);
        let to_client = mitm.chan(0).unwrap();
        let to_server = mitm.chan(1).unwrap();
        for _ in 0..3 {
            let t = read_msg(&to_client, MAX_MSIZE).unwrap();
            write_full(&to_server, &t).unwrap();
            let r = read_msg(&to_server, MAX_MSIZE).unwrap();
            let msg = Msg::decode(&r, MAX_MSIZE).unwrap();
            let out = match msg.fcall {
                Fcall::Rread { data } if data.len() == 64 => Msg {
                    tag: msg.tag,
                    fcall: Fcall::Rread { data: &data[..32] },
                }
                .encode()
                .unwrap(),
                _ => r.clone(),
            };
            write_full(&to_client, &out).unwrap();
        }
    });
    let err = p.mount(c, -1, "#R/mnt", MREPL, "").unwrap_err();
    assert_eq!(err.as_str(), purg_core::auth::NO_SESSION);
    p.close(c).unwrap();
    drop(conn);
    within("the relay ends", move || relay.join().unwrap());
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}

/// Workers answer out of order while one request is blocked; the records
/// still go out in sequence, so the client opens every one.
#[test]
fn concurrent_replies_stay_in_record_order() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let fd = p
        .create("#R/box/pipe", mode::OREAD, purg_styx::DMDIR)
        .unwrap();
    p.close(fd).unwrap();
    p.bind("#|", "#R/box/pipe", MREPL).unwrap();
    let feed = p.open("#R/box/pipe/data1", mode::ORDWR).unwrap();
    let hold = p.open("#R/box/pipe/data", mode::ORDWR).unwrap();
    let (server, b) = spawn_export(&mut p, &mock, 0);
    p.mount(b, -1, "#R/mnt", MREPL | MCREATE, "").unwrap();
    p.close(b).unwrap();

    let fd = p.open("#R/mnt/pipe/data", mode::ORDWR).unwrap();
    let pipe = p.chan(fd).unwrap();
    let reader = {
        let pipe = pipe.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 16];
            let n = pipe.dev.read(&pipe, &mut buf, 0).unwrap();
            buf[..n].to_vec()
        })
    };
    thread::sleep(Duration::from_millis(100));
    let m = mock.clone();
    let mut p = within("requests beside a blocked one", move || {
        enter(&m, &p);
        for _ in 0..20 {
            use_mount(&mut p);
        }
        p
    });
    enter(&mock, &p);
    assert_eq!(p.write(feed, b"late").unwrap(), 4);
    assert_eq!(reader.join().unwrap(), b"late");
    p.close(fd).unwrap();
    drop(pipe);
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    p.close(feed).unwrap();
    p.close(hold).unwrap();
    kprocs_gone(&mock);
}
