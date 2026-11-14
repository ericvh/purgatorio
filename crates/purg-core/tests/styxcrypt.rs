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

/// The public test key, trusted by the exporter (for any user) and by the
/// client (for any server), and held by both: what `/lib/test/keys` holds.
fn install_fixture(p: &mut Prog) {
    p.bind("#R", "/", purg_abi::mount::MAFTER | MCREATE)
        .unwrap();
    for dir in ["/lib", "/lib/test", "/lib/keys"] {
        let fd = p.create(dir, mode::OREAD, purg_styx::DMDIR).unwrap();
        p.close(fd).unwrap();
    }
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let public = purg_core::auth::public_key(&seed);
    let any = format!("* {}\n", purg_auth::hex::encode(&public));
    for (path, bytes) in [
        ("/lib/test/auth.key", seed.as_slice()),
        ("/lib/test/auth.key.pub", public.as_slice()),
        ("/lib/keys/users", any.as_bytes()),
        ("/lib/keys/servers", any.as_bytes()),
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

/// A server that answers "authentication not required" (an export with
/// `-A`) is spoken to in the clear too, by a mount that did not ask for
/// `-A` itself.
#[test]
fn a_server_without_keys_stays_clear() {
    let (mut p, mock) = setup();
    let (server, b) = spawn_export(&mut p, &mock, MNOAUTH);
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

/// An export without `-A` whose namespace trusts no key lets nobody in:
/// not a mount that proves, not one that asks for no proof, and not
/// `fauth` (`docs/auth.md`, "Trust by default"). Nothing of the tree
/// crosses the wire.
#[test]
fn an_export_that_trusts_nothing_lets_nobody_in() {
    use purg_core::auth::NO_TRUST;
    let (mut p, mock) = setup();
    // The client holds the test key; the exporter trusts nothing.
    install_fixture(&mut p);
    p.remove("/lib/keys/users").unwrap();
    for flags in [0, MNOAUTH] {
        let (server, b) = spawn_export(&mut p, &mock, 0);
        let (relay, c) = Relay::new(&mut p, &mock, b, None);
        let r = p.mount(c, -1, "#R/mnt", MREPL | MCREATE | flags, "");
        assert_eq!(r.unwrap_err().as_str(), NO_TRUST, "flags {flags:#x}");
        p.close(c).unwrap();
        let (_, down) = relay.join();
        within("the export ends", move || server.join().unwrap()).unwrap();
        assert!(!contains(&down, b"box"), "flags {flags:#x}");
    }
    let (server, b) = spawn_export(&mut p, &mock, 0);
    assert_eq!(p.fauth(b, "").unwrap_err().as_str(), NO_TRUST);
    p.close(b).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
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
        // The kernel takes the key whatever host its line names: the
        // host is the guest's to check (`docs/auth.md`, "Servers per
        // host").
        let line = format!(
            "box.example {} the other server\n",
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
    let (server, b) = spawn_export(&mut p, &mock, MNOAUTH);
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
    /// One answer for the first read and another for the rest (`None` is
    /// a fresh proof): a server that would show a guest one proof and the
    /// kernel's check another.
    Two(Option<Vec<u8>>, Option<Vec<u8>>),
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
        let mut reads = 0;
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
                    reads += 1;
                    let given = match &answer {
                        Answer::Fresh => None,
                        Answer::Reused(p) => Some(p),
                        Answer::Two(a, _) if reads == 1 => a.as_ref(),
                        Answer::Two(_, b) => b.as_ref(),
                    };
                    proof = given.cloned().unwrap_or_else(|| {
                        purg_auth::server::make(
                            &[purg_core::auth::FIXTURE; 32],
                            None,
                            keys.as_ref().unwrap(),
                        )
                    });
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

/// An export with `ENCRYPT` refuses a version 1 or 2 proof; a version 3
/// mount works.
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
    // Nothing trusted: nobody gets in, encrypted or not.
    let (mut p, mock) = setup();
    let (server, c) = spawn_export(&mut p, &mock, ENCRYPT);
    let r = p.mount(c, -1, "#R/mnt", MREPL, "");
    assert_eq!(r.unwrap_err().as_str(), purg_core::auth::NO_TRUST);
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

/// The auth fd reads at offset 64 as the server's proof, fetched from the
/// server once: every read gives those bytes, and a mount with `MSRVAUTH`
/// checks them, not a second answer. So what a guest checked for the host
/// it dialled (`docs/auth.md`, "Servers per host") is what the kernel
/// verifies; a server cannot show one proof to each.
#[test]
fn the_server_proof_is_read_once() {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let real = a_real_server_proof();
    // The guest reads first; the mount's check must see the same answer.
    for (answer, want) in [
        (
            Answer::Two(Some(real.clone()), None),
            Err(String::from(SERVER_FAILED)),
        ),
        (Answer::Two(None, Some(real.clone())), Ok(())),
    ] {
        let first_bad = matches!(&answer, Answer::Two(Some(_), _));
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, c) = fake_server(&mut p, &mock, answer);
        let afd = p.fauth(c, "").unwrap();
        let mut signed = [0u8; SIGNED];
        assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
        let proof = purg_auth::proof::make(&seed, None, &signed, "inferno", "");
        assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
        {
            // Every read gives the first answer.
            let chan = p.chan(afd).unwrap();
            let at = purg_core::auth::SERVER_PROOF_OFFSET;
            let mut a = vec![0u8; 4096];
            let n = chan.dev.read(&chan, &mut a, at).unwrap();
            a.truncate(n);
            let mut b = vec![0u8; 4096];
            let n = chan.dev.read(&chan, &mut b, at).unwrap();
            b.truncate(n);
            let mut tail = vec![0u8; 4096];
            let n = chan.dev.read(&chan, &mut tail, at + 65).unwrap();
            tail.truncate(n);
            assert_eq!(a, b);
            assert_eq!(tail, a[65..]);
            assert_eq!(a == real, first_bad);
        }
        let r = p.mount(c, afd, "#R/mnt", MREPL | MSRVAUTH, "");
        if r.is_ok() {
            p.unmount(None, "#R/mnt").unwrap();
        }
        assert_eq!(r.map_err(|e| String::from(e.as_str())), want);
        p.close(afd).unwrap();
        p.close(c).unwrap();
        within("the server ends", move || server.join().unwrap());
        kprocs_gone(&mock);
    }
}

/// A conversation whose `fauth` session is still open, here after a
/// refused version 1 proof that left it in the clear, cannot be mounted
/// with `afd = -1` or given to `fauth` again: a second session would race
/// the first one's reader for the replies. Once the auth fd is closed the
/// fd mounts.
#[test]
fn a_live_session_keeps_its_conversation() {
    use purg_abi::error::EINUSE;
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, 0);
    let afd = p.fauth(c, "").unwrap();
    let mut challenge = [0u8; purg_core::auth::CHALLENGE];
    assert_eq!(p.read(afd, &mut challenge).unwrap(), challenge.len());
    let r = p.write(afd, &[0u8; purg_core::auth::PROOF]);
    assert_eq!(r.unwrap_err().as_str(), purg_core::auth::FAILED);
    let r = p.mount(c, -1, "#R/mnt", MREPL, "");
    assert_eq!(r.unwrap_err().as_str(), EINUSE);
    assert_eq!(p.fauth(c, "").unwrap_err().as_str(), EINUSE);
    p.close(afd).unwrap();
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}

// Post-quantum exchange (`docs/auth.md`, "Post-quantum options").

use purg_abi::mount::MPQ;
use purg_core::auth::{NO_PQ, OFFER, OFFER_PQ, PQ_REQUIRED, SIGNED_PQ};

/// A mount with `MPQ` takes the hybrid exchange: the long offer comes
/// down, the proof carries the ML-KEM ciphertext, and nothing after it is
/// in the clear.
#[test]
fn a_pq_mount_is_encrypted() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let (relay, c) = Relay::new(&mut p, &mock, b, None);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE | MPQ, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    let (up, down) = relay.join();
    within("the export ends", move || server.join().unwrap()).unwrap();
    for (dir, log) in [("up", &up), ("down", &down)] {
        assert!(!contains(log, NAME), "{dir}: the name went in the clear");
        assert!(!contains(log, DATA), "{dir}: the data went in the clear");
    }
    // Frame 2 down is the Rread of the long offer, frame 3 up the
    // version 4 proof: size[4] type[1] tag[2] fid[4] offset[8] count[4].
    let rread = frame_end(&down, 2).unwrap() - frame_end(&down, 1).unwrap();
    assert_eq!(rread, 4 + 1 + 2 + 4 + OFFER_PQ);
    let start = frame_end(&up, 2).unwrap();
    assert_eq!(up[start + 23], purg_auth::proof::V4);
    kprocs_gone(&mock);
}

/// The same through `fauth`: a read of SIGNED_PQ bytes gives the long
/// offer with the kernel's key and ciphertext, a version 4 proof over
/// them switches the conversation, and a mount with `MPQ` takes it. After
/// a version 3 proof a mount with `MPQ` is refused.
#[test]
fn fauth_with_a_version_4_proof() {
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    for v4 in [true, false] {
        let (mut p, mock) = setup();
        install_fixture(&mut p);
        let (server, c) = spawn_export(&mut p, &mock, 0);
        let afd = p.fauth(c, "").unwrap();
        let mut signed = vec![
            0u8;
            if v4 {
                SIGNED_PQ
            } else {
                purg_core::auth::SIGNED
            }
        ];
        assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
        // Every later read gives the same bytes; a shorter one cannot.
        let mut again = vec![0u8; SIGNED_PQ];
        assert_eq!(p.pread(afd, &mut again, 0).unwrap(), signed.len());
        assert_eq!(&again[..signed.len()], &signed[..]);
        if v4 {
            let mut short = [0u8; purg_core::auth::SIGNED];
            assert_eq!(
                p.pread(afd, &mut short, 0).unwrap_err().as_str(),
                purg_abi::error::EBADARG
            );
        }
        let proof = purg_auth::proof::make(&seed, None, &signed, "inferno", "");
        assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
        let r = p.mount(c, afd, "#R/mnt", MREPL | MCREATE | MPQ, "");
        if v4 {
            r.unwrap();
            use_mount(&mut p);
            p.unmount(None, "#R/mnt").unwrap();
        } else {
            assert_eq!(r.unwrap_err().as_str(), PQ_REQUIRED);
        }
        p.close(afd).unwrap();
        p.close(c).unwrap();
        within("the export ends", move || server.join().unwrap()).unwrap();
        kprocs_gone(&mock);
    }
}

/// An export with `MPQ` refuses the classical exchange, from the kernel's
/// mount and from a version 3 proof through `fauth`, and takes the hybrid
/// one. A classical client of an export without it works as before.
#[test]
fn an_export_can_require_pq() {
    assert_eq!(mount_with(0, |_| {}), Ok(()));
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, c) = spawn_export(&mut p, &mock, MPQ);
    let r = p.mount(c, -1, "#R/mnt", MREPL, "");
    assert_eq!(r.unwrap_err().as_str(), PQ_REQUIRED);
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    let (server, c) = spawn_export(&mut p, &mock, MPQ);
    let afd = p.fauth(c, "").unwrap();
    let mut signed = [0u8; purg_core::auth::SIGNED];
    assert_eq!(p.read(afd, &mut signed).unwrap(), signed.len());
    let proof = purg_auth::proof::make(&seed, None, &signed, "inferno", "");
    assert_eq!(p.write(afd, &proof).unwrap_err().as_str(), PQ_REQUIRED);
    let r = p.mount(c, afd, "#R/mnt", MREPL, "");
    assert_eq!(r.unwrap_err().as_str(), purg_core::auth::REQUIRED);
    p.close(afd).unwrap();
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    let (server, c) = spawn_export(&mut p, &mock, MPQ);
    p.mount(c, -1, "#R/mnt", MREPL | MCREATE | MPQ, "").unwrap();
    p.close(c).unwrap();
    use_mount(&mut p);
    p.unmount(None, "#R/mnt").unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    // `-A` with `MPQ` is a usage error on both sides, and a mount with
    // `MPQ` of a server that asks for no proof is refused.
    let (server, c) = spawn_export(&mut p, &mock, MPQ | MNOAUTH);
    let served = within("the export ends", move || server.join().unwrap());
    assert_eq!(served, Err(String::from(purg_abi::error::EBADARG)));
    p.close(c).unwrap();
    let (server, c) = spawn_export(&mut p, &mock, MNOAUTH);
    let r = p.mount(c, -1, "#R/mnt", MREPL | MPQ | MNOAUTH, "");
    assert_eq!(r.unwrap_err().as_str(), purg_abi::error::EBADARG);
    let r = p.mount(c, -1, "#R/mnt", MREPL | MPQ, "");
    assert_eq!(r.unwrap_err().as_str(), NO_PQ);
    p.close(c).unwrap();
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
}

/// A man in the middle that relays `n` request and reply pairs, each
/// passed through `rewrite(request?, message)`, then stops.
fn mitm(
    p: &mut Prog,
    mock: &Arc<Mock>,
    server: i32,
    n: usize,
    rewrite: fn(bool, Vec<u8>) -> Vec<u8>,
) -> (thread::JoinHandle<()>, i32) {
    let [c, r] = p.pipe().unwrap();
    let mut mitm = p
        .spawn(purg_abi::pctl::NEWFD, &[(r, 0), (server, 1)])
        .unwrap();
    mitm.started();
    p.close(r).unwrap();
    p.close(server).unwrap();
    let m = mock.clone();
    let relay = thread::spawn(move || {
        use purg_core::rw::{read_msg, write_full};
        use purg_styx::MAX_MSIZE;
        enter(&m, &mitm);
        let to_client = mitm.chan(0).unwrap();
        let to_server = mitm.chan(1).unwrap();
        for _ in 0..n {
            let Ok(t) = read_msg(&to_client, MAX_MSIZE) else {
                return;
            };
            if write_full(&to_server, &rewrite(true, t)).is_err() {
                return;
            }
            let Ok(r) = read_msg(&to_server, MAX_MSIZE) else {
                return;
            };
            if write_full(&to_client, &rewrite(false, r)).is_err() {
                return;
            }
        }
    });
    (relay, c)
}

fn rewrite_msg(bytes: &[u8], f: impl FnOnce(purg_styx::Fcall<'_>) -> Option<Vec<u8>>) -> Vec<u8> {
    use purg_styx::{MAX_MSIZE, Msg};
    let msg = Msg::decode(bytes, MAX_MSIZE).unwrap();
    let tag = msg.tag;
    match f(msg.fcall) {
        Some(data) => Msg {
            tag,
            fcall: purg_styx::Fcall::Rread { data: &data },
        }
        .encode()
        .unwrap(),
        None => bytes.to_vec(),
    }
}

/// Runs a mount with `MPQ` through [`mitm`] and returns its error.
fn pq_mount_through(n: usize, rewrite: fn(bool, Vec<u8>) -> Vec<u8>) -> String {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, &mock, 0);
    let (relay, c) = mitm(&mut p, &mock, b, n, rewrite);
    let conn = p.chan(c).unwrap();
    let err = p.mount(c, -1, "#R/mnt", MREPL | MPQ, "").unwrap_err();
    p.close(c).unwrap();
    drop(conn);
    within("the relay ends", move || relay.join().unwrap());
    within("the export ends", move || server.join().unwrap()).unwrap();
    kprocs_gone(&mock);
    String::from(err.as_str())
}

/// Cutting the ML-KEM key out of the offer, as a server that predates
/// version 4 would answer, fails a mount that requires the hybrid
/// exchange instead of falling back to the classical one.
#[test]
fn a_stripped_pq_offer_is_refused() {
    let err = pq_mount_through(3, |up, m| {
        if up {
            return m;
        }
        rewrite_msg(&m, |f| match f {
            purg_styx::Fcall::Rread { data } if data.len() == OFFER_PQ => {
                Some(data[..OFFER].to_vec())
            }
            _ => None,
        })
    });
    assert_eq!(err, NO_PQ);
}

/// The same by asking the server for the short offer: the request is in
/// the clear, but the client still sees no ML-KEM key and refuses.
#[test]
fn a_shortened_offer_request_is_refused() {
    let err = pq_mount_through(3, |up, m| {
        if !up {
            return m;
        }
        let mut m = m;
        // Tread: size[4] type[1] tag[2] fid[4] offset[8] count[4].
        if m.get(4) == Some(&purg_styx::kind::TREAD) && m.len() == 23 {
            m[19..23].copy_from_slice(&(OFFER as u32).to_le_bytes());
        }
        m
    });
    assert_eq!(err, NO_PQ);
}

/// Stripping the ML-KEM ciphertext from a version 4 proof, to pass it on
/// as a version 3 proof, fails: the signature covers the version 4
/// context, the long offer and the ciphertext.
#[test]
fn a_proof_stripped_to_version_3_is_refused() {
    let err = pq_mount_through(4, |up, m| {
        use purg_auth::proof::{CT, V3, V4};
        // Twrite: size[4] type[1] tag[2] fid[4] offset[8] count[4] data.
        if !up || m.get(4) != Some(&purg_styx::kind::TWRITE) || m.get(23) != Some(&V4) {
            return m;
        }
        let head = 23 + 2 + purg_core::auth::SIGNATURE + purg_core::auth::PUBLIC;
        let mut data = m[23..head].to_vec();
        data[0] = V3;
        data.extend_from_slice(&m[head + CT..]);
        let mut out = m[..23].to_vec();
        out[19..23].copy_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let size = out.len() as u32;
        out[..4].copy_from_slice(&size.to_le_bytes());
        out
    });
    assert_eq!(err, purg_core::auth::FAILED);
}
