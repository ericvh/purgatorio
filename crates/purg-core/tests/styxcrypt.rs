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

// Server authentication (issue #105): the server proves itself after the
// client, with a proof over the session's keys read from the auth fid at
// offset 64; `MSRVAUTH` makes the client require it.

use purg_abi::export::ENCRYPT;
use purg_abi::mount::MSRVAUTH;
use purg_core::auth::{ENCRYPTED, ENCRYPTION_REQUIRED, SERVER_FAILED, SERVER_NONE, SIGNED};

/// Another key pair, which neither side trusts unless a test says so.
const OTHER: [u8; 32] = [0x55; 32];

fn write_file(p: &mut Prog, path: &str, bytes: &[u8]) {
    let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, bytes).unwrap(), bytes.len());
    p.close(fd).unwrap();
}

fn mkdirs(p: &mut Prog, dirs: &[&str]) {
    for dir in dirs {
        if let Ok(fd) = p.create(dir, mode::OREAD, purg_styx::DMDIR) {
            p.close(fd).unwrap();
        }
    }
}

/// Gives the user `inferno` the key [`OTHER`]: both the mount and the
/// export then sign with it, and the export trusts it for the client.
fn install_other_user_key(p: &mut Prog) {
    mkdirs(p, &["/usr", "/usr/inferno"]);
    write_file(p, "/usr/inferno/key", &OTHER);
    write_file(
        p,
        "/usr/inferno/key.pub",
        &purg_core::auth::public_key(&OTHER),
    );
}

/// Mounts the export with `flags` and returns the mount's result, after
/// using it when it worked.
fn mount_with(flags: u32, prepare: impl FnOnce(&mut Prog)) -> Result<(), String> {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    prepare(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let r = p.mount(b, -1, "#R/mnt", MREPL | MCREATE | flags, "");
    p.close(b).unwrap();
    if r.is_ok() {
        use_mount(&mut p);
        p.unmount(None, "#R/mnt").unwrap();
    }
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
    r.map_err(|e| String::from(e.as_str()))
}

/// The export proves itself with the test key, which the client trusts as
/// a server's: a mount that requires that works.
#[test]
fn a_mount_requiring_the_server_accepts_the_real_one() {
    assert_eq!(mount_with(MSRVAUTH, |_| {}), Ok(()));
}

/// An export whose key the client does not trust as a server's is refused
/// with `MSRVAUTH` (and accepted without it); listed in
/// `/lib/keys/servers`, the same key is accepted.
#[test]
fn an_impostor_server_is_refused() {
    assert_eq!(
        mount_with(MSRVAUTH, install_other_user_key),
        Err(String::from(SERVER_FAILED))
    );
    assert_eq!(mount_with(0, install_other_user_key), Ok(()));
    let listed = |p: &mut Prog| {
        install_other_user_key(p);
        mkdirs(p, &["/lib/keys"]);
        let line = format!(
            "{} the other server\n",
            purg_auth::hex::encode(&purg_core::auth::public_key(&OTHER))
        );
        write_file(p, "/lib/keys/servers", line.as_bytes());
    };
    assert_eq!(mount_with(MSRVAUTH, listed), Ok(()));
}

/// A server that asks for no proof cannot prove itself, and `-A` with
/// `MSRVAUTH` is a contradiction.
#[test]
fn a_server_without_keys_cannot_prove_itself() {
    let (mut p, mock) = setup();
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let r = p.mount(b, -1, "#R/mnt", MREPL | MSRVAUTH | MNOAUTH, "");
    assert_eq!(r.unwrap_err().as_str(), purg_abi::error::EBADARG);
    let r = p.mount(b, -1, "#R/mnt", MREPL | MSRVAUTH, "");
    assert_eq!(r.unwrap_err().as_str(), SERVER_NONE);
    p.close(b).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}

/// Through `fauth` the kernel checks the server's proof when the mount
/// with the auth fd asks for it; a version 2 proof leaves nothing to
/// check.
#[test]
fn fauth_mount_checks_the_server() {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    for v3 in [true, false] {
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, c) = spawn_export(&mut p, &mock, 0);
        let afd = p.fauth(c, "").unwrap();
        let proof = if v3 {
            let mut signed = [0u8; SIGNED];
            assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
            purg_auth::proof::make(&seed, None, &signed, "inferno", "")
        } else {
            let mut challenge = [0u8; purg_core::auth::CHALLENGE];
            assert_eq!(p.read(afd, &mut challenge).unwrap(), challenge.len());
            purg_auth::proof::make(&seed, None, &challenge, "inferno", "")
        };
        assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
        let r = p.mount(c, afd, "#R/mnt", MREPL | MCREATE | MSRVAUTH, "");
        if v3 {
            r.unwrap();
            use_mount(&mut p);
            p.unmount(None, "#R/mnt").unwrap();
        } else {
            assert_eq!(r.unwrap_err().as_str(), SERVER_NONE);
        }
        p.close(afd).unwrap();
        p.close(c).unwrap();
        within("the export ends", move || server.join().unwrap()).unwrap();
        kprocs_gone(&mock);
    }
}

/// What the fake server below answers at offset 64 of its auth fid.
enum Answer {
    /// A proof it makes for this session with the test key (which the
    /// client trusts): the control.
    Fresh,
    /// A proof the real server made for another session.
    Reused(Vec<u8>),
}

/// A server of its own that takes any proof, holds its end of the session
/// (so it can seal what it sends), and answers the read of its proof
/// with `answer`. It serves an attach and clunks, so a mount can work.
fn fake_server(p: &mut Prog, mock: &Arc<Mock>, answer: Answer) -> (thread::JoinHandle<()>, i32) {
    use purg_core::rw::{read_msg, read_sealed, write_full, write_sealed};
    use purg_styx::{Fcall, MAX_MSIZE, Msg, QTAUTH, QTDIR, Qid};
    let [a, b] = p.pipe().unwrap();
    let mut prog = p.spawn(purg_abi::pctl::NEWFD, &[(a, 0)]).unwrap();
    prog.started();
    p.close(a).unwrap();
    let m = mock.clone();
    let t = thread::spawn(move || {
        enter(&m, &prog);
        let conn = prog.chan(0).unwrap();
        let kx = purg_auth::session::Server::new([1; 32], [2; 32]);
        let mut ends: Option<(purg_auth::record::Sealer, purg_auth::record::Opener)> = None;
        let mut keys = None;
        loop {
            let got = match &mut ends {
                Some((_, o)) => read_sealed(&conn, o, MAX_MSIZE),
                None => read_msg(&conn, MAX_MSIZE),
            };
            let Ok(buf) = got else { return };
            let msg = Msg::decode(&buf, MAX_MSIZE).unwrap();
            let qid = |qtype| Qid {
                path: 0,
                vers: 0,
                qtype,
            };
            let proof;
            let fcall = match msg.fcall {
                Fcall::Tversion { msize, version } => Fcall::Rversion { msize, version },
                Fcall::Tauth { .. } => Fcall::Rauth { aqid: qid(QTAUTH) },
                Fcall::Tread { offset: 0, .. } => Fcall::Rread { data: &kx.offer },
                Fcall::Tread { .. } => {
                    proof = match &answer {
                        Answer::Fresh => purg_auth::server::make(
                            &[purg_core::auth::FIXTURE; 32],
                            None,
                            keys.as_ref().unwrap(),
                        ),
                        Answer::Reused(p) => p.clone(),
                    };
                    Fcall::Rread { data: &proof }
                }
                Fcall::Twrite { data, .. } => {
                    keys = Some(kx.keys("inferno", "", data).unwrap());
                    Fcall::Rwrite {
                        count: data.len() as u32,
                    }
                }
                Fcall::Tattach { .. } => Fcall::Rattach { qid: qid(QTDIR) },
                Fcall::Tclunk { .. } => Fcall::Rclunk,
                _ => Fcall::Rerror { ename: "no" },
            };
            let out = Msg {
                tag: msg.tag,
                fcall,
            }
            .encode()
            .unwrap();
            let sent = match &mut ends {
                Some((s, _)) => write_sealed(&conn, Some(s), &out),
                None => write_full(&conn, &out),
            };
            if sent.is_err() {
                return;
            }
            // The proof's reply was the last in the clear.
            if ends.is_none()
                && let Some(k) = &keys
            {
                ends = Some(k.server());
            }
        }
    });
    (t, b)
}

/// The real server's proof for one session, as a client that reads its
/// auth fd at offset 64 sees it.
fn a_real_server_proof() -> Vec<u8> {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, 0);
    let afd = p.fauth(c, "").unwrap();
    let mut signed = [0u8; SIGNED];
    assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
    let proof = purg_auth::proof::make(&seed, None, &signed, "inferno", "");
    assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
    let chan = p.chan(afd).unwrap();
    let mut buf = vec![0u8; 4096];
    let n = chan
        .dev
        .read(&chan, &mut buf, purg_core::auth::SERVER_PROOF_OFFSET)
        .unwrap();
    buf.truncate(n);
    drop(chan);
    p.close(afd).unwrap();
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
    buf
}

/// A man in the middle that holds its own end of the session, and so can
/// seal what it sends, cannot pass off a proof the real server made for
/// another session (here one it got as a client of the real server): the
/// proof signs that session's transcript and bound key. The same server
/// with a proof of its own for this session gets in.
#[test]
fn a_server_proof_from_another_session_is_refused() {
    let real = a_real_server_proof();
    assert_eq!(real.len(), 1 + 64 + 32);
    for (answer, want) in [
        (Answer::Reused(real), Err(String::from(SERVER_FAILED))),
        (Answer::Fresh, Ok(())),
    ] {
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, c) = fake_server(&mut p, &mock, answer);
        let r = p.mount(c, -1, "#R/mnt", MREPL | MSRVAUTH, "");
        p.close(c).unwrap();
        if r.is_ok() {
            p.unmount(None, "#R/mnt").unwrap();
        }
        assert_eq!(r.map_err(|e| String::from(e.as_str())), want);
        within("the server ends", move || server.join().unwrap());
        kprocs_gone(&mock);
    }
}

/// An export with `ENCRYPT` refuses a version 1 or 2 proof, and an attach
/// without a proof when it trusts nothing; a version 3 mount works.
#[test]
fn an_export_can_require_encryption() {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    for version in [1, 2] {
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, c) = spawn_export(&mut p, &mock, ENCRYPT);
        let afd = p.fauth(c, "").unwrap();
        let mut challenge = [0u8; purg_core::auth::CHALLENGE];
        assert_eq!(p.read(afd, &mut challenge).unwrap(), challenge.len());
        let proof = if version == 2 {
            purg_auth::proof::make(&seed, None, &challenge, "inferno", "")
        } else {
            let mut v1 = purg_core::auth::sign(&seed, &challenge).to_vec();
            v1.extend_from_slice(&purg_core::auth::public_key(&seed));
            v1
        };
        let r = p.write(afd, &proof);
        assert_eq!(r.unwrap_err().as_str(), ENCRYPTION_REQUIRED, "v{version}");
        let r = p.mount(c, afd, "#R/mnt", MREPL, "");
        assert_eq!(r.unwrap_err().as_str(), purg_core::auth::REQUIRED);
        p.close(afd).unwrap();
        p.close(c).unwrap();
        within("the export ends", move || server.join().unwrap()).unwrap();
        kprocs_gone(&mock);
    }
    // Version 3, through the kernel's own mount.
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, ENCRYPT);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
    // Nothing trusted, so nothing can be encrypted.
    let (mut p, mock) = setup();
    let (server, c) = spawn_export(&mut p, &mock, ENCRYPT);
    let r = p.mount(c, -1, "#R/mnt", MREPL, "");
    assert_eq!(r.unwrap_err().as_str(), ENCRYPTION_REQUIRED);
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    // `-A` and `-e` together.
    let (server, c) = spawn_export(&mut p, &mock, ENCRYPT | MNOAUTH);
    let served = within("the export ends", move || server.join().unwrap());
    assert_eq!(served, Err(String::from(purg_abi::error::EBADARG)));
    p.close(c).unwrap();
    kprocs_gone(&mock);
}

/// A version 3 proof switches the conversation to records even when it is
/// refused. Starting it again in the clear on the same fd fails with a
/// clear error, before and after the auth fd is closed.
#[test]
fn a_refused_version_3_proof_leaves_the_fd_encrypted() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, 0);
    let afd = p.fauth(c, "").unwrap();
    let mut signed = [0u8; SIGNED];
    assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
    // Signed by a key the export does not trust.
    let proof = purg_auth::proof::make(&OTHER, None, &signed, "inferno", "");
    let r = p.write(afd, &proof);
    assert_eq!(r.unwrap_err().as_str(), purg_core::auth::FAILED);
    let again = |p: &mut Prog| {
        let r = p.mount(c, -1, "#R/mnt", MREPL, "");
        assert_eq!(r.unwrap_err().as_str(), ENCRYPTED);
        assert_eq!(p.fauth(c, "").unwrap_err().as_str(), ENCRYPTED);
    };
    again(&mut p);
    p.close(afd).unwrap();
    again(&mut p);
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}

/// Every read of the offer, from any thread, gives the same ephemeral key,
/// so a read beside the proof's write cannot change the key under it.
#[test]
fn the_offer_reads_the_same_from_every_thread() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, 0);
    let afd = p.fauth(c, "").unwrap();
    let chan = p.chan(afd).unwrap();
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let chan = chan.clone();
            thread::spawn(move || {
                let mut signed = [0u8; SIGNED];
                assert_eq!(chan.dev.read(&chan, &mut signed, 0).unwrap(), SIGNED);
                signed
            })
        })
        .collect();
    let got: Vec<[u8; SIGNED]> = readers.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(got.windows(2).all(|w| w[0] == w[1]));
    let mut signed = [0u8; SIGNED];
    assert_eq!(p.read(afd, &mut signed).unwrap(), SIGNED);
    assert_eq!(signed, got[0]);
    drop(chan);
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let proof = purg_auth::proof::make(&seed, None, &signed, "inferno", "");
    assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
    p.mount(c, afd, "#R/mnt", MREPL | MCREATE | MSRVAUTH, "")
        .unwrap();
    p.close(afd).unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}
