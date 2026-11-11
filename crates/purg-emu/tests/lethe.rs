//! Lethe end to end (`docs/lethe.md`): `lethed` serving a scratch tree on
//! loopback, and the `lethe` command and `#λ` in the emu on both WASM
//! backends reading it. A test client built on `purg_lethe` checks what
//! the guest cannot: tampered and replayed requests. Requires `cargo
//! xtask build-root` first.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use purg_auth::cert::Cert;
use purg_auth::keys::AuthInfo;
use purg_lethe::client::{self, Creds};
use purg_lethe::msg::{Frame, Msg, Op, Owned};

const VMS: [&str; 2] = ["wasmtime", "wasmi"];
/// The public test seed, `root/lib/test/auth.key`.
const TEST_SEED: [u8; 32] = [0x42; 32];
const SIGNER: [u8; 32] = [5; 32];
const OTHER_SIGNER: [u8; 32] = [6; 32];
const USER_SEED: [u8; 32] = [7; 32];

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/lethe.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

/// A scratch directory under `root/tmp`, so the guest sees it as
/// `/tmp/<name>`, with a tree to serve in `srv` and a mount point `n`.
struct Scratch {
    host: PathBuf,
    guest: String,
}

const HELLO: &str = "hello from lethe\n";
const MANY: usize = 300;

impl Scratch {
    fn new(test: &str) -> Scratch {
        let name = format!("lethe-{}-{test}", std::process::id());
        let host = root().join("tmp").join(&name);
        let _ = std::fs::remove_dir_all(&host);
        let srv = host.join("srv");
        std::fs::create_dir_all(srv.join("sub")).unwrap();
        std::fs::create_dir_all(srv.join("many")).unwrap();
        std::fs::create_dir_all(host.join("n")).unwrap();
        std::fs::write(srv.join("hello.txt"), HELLO).unwrap();
        std::fs::write(srv.join("sub/inner"), "inner file\n").unwrap();
        for i in 0..MANY {
            std::fs::write(srv.join(format!("many/f{i:03}")), "").unwrap();
        }
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(srv.join("big"), big).unwrap();
        // A link inside the tree is followed; one out of it is not there.
        std::os::unix::fs::symlink("hello.txt", srv.join("in")).unwrap();
        std::os::unix::fs::symlink(root().join("lib/probe/hello.txt"), srv.join("out")).unwrap();
        std::os::unix::fs::symlink("../..", srv.join("up")).unwrap();
        Scratch {
            guest: format!("/tmp/{name}"),
            host,
        }
    }

    fn srv(&self) -> PathBuf {
        self.host.join("srv")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.host);
        }
    }
}

/// A running `lethed`; killed when dropped. Its `-v` log is collected.
struct Lethed {
    child: Child,
    port: u16,
    log: Arc<Mutex<String>>,
}

impl Lethed {
    fn start(args: &[&str], tree: &Path) -> Lethed {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lethed"))
            .arg("-v")
            .args(args)
            .arg(tree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lethed");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .strip_prefix("lethed: listening on ")
            .unwrap_or_else(|| panic!("lethed said {line:?}"));
        let port = addr.rsplit(':').next().unwrap().parse().unwrap();
        let log = Arc::new(Mutex::new(String::new()));
        let err = child.stderr.take().unwrap();
        let l = log.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines() {
                let Ok(line) = line else { break };
                let mut l = l.lock().unwrap();
                l.push_str(&line);
                l.push('\n');
            }
        });
        Lethed { child, port, log }
    }

    fn addr(&self) -> String {
        format!("tcp!127.0.0.1!{}", self.port)
    }

    /// Log lines containing `s`, after a moment for the log to drain.
    fn count(&self, s: &str) -> usize {
        std::thread::sleep(Duration::from_millis(100));
        self.log
            .lock()
            .unwrap()
            .lines()
            .filter(|l| l.contains(s))
            .count()
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Lethed {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn test_key() -> String {
    root().join("lib/test/auth.key.pub").display().to_string()
}

/// Runs `script` in the emu; returns stdout and whether it succeeded.
fn emu(vm: &str, script: &str) -> (String, bool) {
    let o = Command::new(env!("CARGO_BIN_EXE_emu"))
        .arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .args(["sh", "-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("run emu");
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        o.status.success(),
    )
}

/// Runs `script` in the emu, calling `at` when it prints `ready` and then
/// sending it a line, which `read < /dev/cons` in the script waits for.
fn emu_paused(vm: &str, script: &str, at: impl FnOnce()) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
        .arg("-r")
        .arg(root())
        .arg(format!("--vm={vm}"))
        .args(["sh", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run emu");
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if line == "ready" {
                let _ = tx.send(());
            }
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    rx.recv_timeout(Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("{vm}: the script never got ready"));
    at();
    child.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let status = child.wait().unwrap();
    let all = reader.join().unwrap();
    assert!(status.success(), "{vm}: {all}");
    all
}

#[test]
fn read_stat_readdir_and_names() {
    let s = Scratch::new("basic");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe {addr} {g}/n || exit
cat {g}/n/hello.txt {g}/n/in
ls {g}/n
ls {g}/n/many | wc -l
ls {g}/n/many/f299
cat {g}/n/sub/inner
cat {g}/n/nope
cat {g}/n/out
ls {g}/n/up
cat {g}/n/hello.txt/x
echo x > {g}/n/hello.txt
cmp {g}/n/big {g}/srv/big && echo same
lethe {addr} {g}/n /sub && ls {g}/n
lethe {addr} {g}/n /nope",
        addr = l.addr(),
    );
    let want = format!(
        "{HELLO}{HELLO}{g}/n/big
{g}/n/hello.txt
{g}/n/in
{g}/n/many
{g}/n/sub
    300
{g}/n/many/f299
inner file
cat: {g}/n/nope: file does not exist
cat: {g}/n/out: file does not exist
ls: {g}/n/up: file does not exist
cat: {g}/n/hello.txt/x: not a directory
sh: {g}/n/hello.txt: permission denied
same
{g}/n/inner
lethe: file does not exist
"
    );
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert_eq!(out, want, "{vm}");
    }
    // The 300 entries came in pages.
    assert!(l.count("readdir /many") >= 2 * 3, "{}", l.log());
    // Two runs, each with a ticket for the root, one for /sub and one for
    // /nope, which then is not there.
    assert_eq!(l.count("ticket for"), 6, "{}", l.log());
    assert_eq!(l.count("prefix /sub"), 2);
}

#[test]
fn a_file_changed_underneath_is_not_served_stale() {
    let s = Scratch::new("changed");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe {} {g}/n || exit
cat {g}/n/hello.txt
cat {g}/n/hello.txt
echo ready
read < /dev/cons > /dev/null
cat {g}/n/hello.txt
ls -l {g}/n/hello.txt",
        l.addr()
    );
    for (i, vm) in VMS.into_iter().enumerate() {
        std::fs::write(s.srv().join("hello.txt"), HELLO).unwrap();
        let changed = format!("changed underneath, {i}\n");
        let out = emu_paused(vm, &script, || {
            std::fs::write(s.srv().join("hello.txt"), &changed).unwrap();
        });
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(HELLO.trim_end()), "{vm}: {out}");
        assert_eq!(lines.next(), Some(HELLO.trim_end()), "{vm}: {out}");
        assert_eq!(lines.next(), Some("ready"), "{vm}: {out}");
        assert_eq!(lines.next(), Some(changed.trim_end()), "{vm}: {out}");
        let ls = lines.next().unwrap_or_default();
        assert!(ls.contains(&format!(" {} ", changed.len())), "{vm}: {ls}");
    }
}

/// `lethed` killed and started again on the same port while a file is
/// open and half read: the reader carries on. With the same ticket key
/// the old ticket still works; with a new one the client gets another.
#[test]
fn lethed_restarted_mid_read() {
    let s = Scratch::new("restart");
    let tk = s.host.join("ticketkey");
    std::fs::write(&tk, [9u8; 32]).unwrap();
    let tk = tk.display().to_string();
    let key = test_key();
    let g = &s.guest;
    for vm in VMS {
        let first = Lethed::start(&["-a", "127.0.0.1:0", "-t", &tk, "-k", &key], &s.srv());
        let port = first.port;
        let script = format!(
            "lethe {} {g}/n || exit
{{read 8192 > {g}/part1; echo ready; read < /dev/cons > /dev/null; cat > {g}/part2}} < {g}/n/big
echo ready
read < /dev/cons > /dev/null
cat {g}/n/big > {g}/part3
echo done",
            first.addr()
        );
        let addr = format!("127.0.0.1:{port}");
        let mut second = None;
        let mut third = None;
        let mut child = Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(root())
            .arg(format!("--vm={vm}"))
            .args(["sh", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut first = Some(first);
        let mut all = String::new();
        let mut readies = 0;
        let mut line = String::new();
        while out.read_line(&mut line).unwrap() > 0 {
            all.push_str(&line);
            if line == "ready\n" {
                readies += 1;
                if readies == 1 {
                    // Same ticket key: the ticket survives the restart.
                    drop(first.take());
                    second = Some(Lethed::start(
                        &["-a", &addr, "-t", &tk, "-k", &key],
                        &s.srv(),
                    ));
                } else {
                    // A fresh key: the client authenticates again.
                    let s2: Lethed = second.take().unwrap();
                    assert!(s2.count("read /big") > 0, "{vm}: {}", s2.log());
                    assert_eq!(s2.count("ticket for"), 0, "{vm}: {}", s2.log());
                    drop(s2);
                    third = Some(Lethed::start(&["-a", &addr, "-k", &key], &s.srv()));
                }
                stdin.write_all(b"go\n").unwrap();
            }
            line.clear();
        }
        assert!(child.wait().unwrap().success(), "{vm}: {all}");
        assert!(all.ends_with("done\n"), "{vm}: {all}");
        let big = std::fs::read(s.srv().join("big")).unwrap();
        let mut got = std::fs::read(s.host.join("part1")).unwrap();
        assert_eq!(got.len(), 8192, "{vm}");
        got.extend(std::fs::read(s.host.join("part2")).unwrap());
        assert!(got == big, "{vm}: the read across the restart differs");
        assert!(std::fs::read(s.host.join("part3")).unwrap() == big, "{vm}");
        let t = third.take().unwrap();
        assert_eq!(t.count("ticket for"), 1, "{vm}: {}", t.log());
        assert!(
            t.count("refused: unknown ticket key") >= 1,
            "{vm}: {}",
            t.log()
        );
    }
}

#[test]
fn an_expired_ticket_is_renewed() {
    let s = Scratch::new("expiry");
    // Tickets last 6 s and are renewed 5 s before they expire.
    let l = Lethed::start(
        &["-a", "127.0.0.1:0", "-e", "6", "-k", &test_key()],
        &s.srv(),
    );
    let g = &s.guest;
    let script = format!(
        "lethe {} {g}/n || exit
cat {g}/n/hello.txt
sleep 2
cat {g}/n/hello.txt",
        l.addr()
    );
    let mut tickets = 0;
    for vm in VMS {
        let (out, ok) = emu(vm, &script);
        assert!(ok, "{vm}: {out}");
        assert_eq!(out, format!("{HELLO}{HELLO}"), "{vm}");
        tickets += 2;
        assert!(l.count("ticket for") >= tickets, "{vm}: {}", l.log());
    }
}

fn authinfo(path: &Path, user: &str, signer: &[u8; 32]) {
    let mut cert = Cert::new(user, purg_auth::public_key(&USER_SEED), 0).unwrap();
    cert.sign(signer).unwrap();
    let info = AuthInfo {
        seed: USER_SEED,
        signer: purg_auth::public_key(signer),
        cert,
    };
    std::fs::write(path, info.encode()).unwrap();
}

#[test]
fn untrusted_clients_are_refused() {
    let s = Scratch::new("trust");
    let signers = s.host.join("signers");
    std::fs::write(
        &signers,
        purg_auth::keys::list_line(&purg_auth::public_key(&SIGNER), "test"),
    )
    .unwrap();
    authinfo(&s.host.join("good"), "glenda", &SIGNER);
    authinfo(&s.host.join("other"), "glenda", &OTHER_SIGNER);
    let revoked = s.host.join("revoked");
    let l = Lethed::start(
        &[
            "-a",
            "127.0.0.1:0",
            "-s",
            signers.to_str().unwrap(),
            "-r",
            revoked.to_str().unwrap(),
        ],
        &s.srv(),
    );
    let g = &s.guest;
    let a = l.addr();
    let script = format!(
        "lethe -u glenda -k {g}/good {a} {g}/n && cat {g}/n/hello.txt
lethe -u glenda -k {g}/other {a} {g}/n
lethe -u glenda {a} {g}/n
lethe -u bob -k {g}/good {a} {g}/n
auth/getpk {g}/good > {g}/revoked
lethe -u glenda -k {g}/good {a} {g}/n
rm {g}/revoked"
    );
    let want = format!(
        "{HELLO}lethe: untrusted signer
lethe: authentication failed
lethe: certificate name mismatch
lethe: certificate revoked
"
    );
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert_eq!(out, want, "{vm}");
    }
    assert!(l.count("refused: untrusted signer") >= 2);
}

/// Copies bytes both ways between each connection to its port and
/// `to`, recording everything.
fn relay(to: u16) -> (u16, Arc<Mutex<Vec<u8>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(c) = c else { break };
            let Ok(u) = TcpStream::connect(("127.0.0.1", to)) else {
                continue;
            };
            for (mut a, mut b) in [(c.try_clone().unwrap(), u.try_clone().unwrap()), (u, c)] {
                let s = s.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = a.read(&mut buf) {
                        if n == 0 || b.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        s.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                    let _ = b.shutdown(std::net::Shutdown::Both);
                });
            }
        }
    });
    (port, seen)
}

#[test]
fn the_wire_is_not_plaintext() {
    let s = Scratch::new("wire");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let (port, seen) = relay(l.port);
    let g = &s.guest;
    let script = format!(
        "lethe tcp!127.0.0.1!{port} {g}/n || exit
cat {g}/n/hello.txt
ls {g}/n/sub
cat {g}/n/secretname"
    );
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert!(out.starts_with(HELLO), "{vm}: {out}");
    }
    let seen = seen.lock().unwrap();
    assert!(seen.len() > 1000, "{}", seen.len());
    for needle in [
        "hello from lethe",
        "hello.txt",
        "inner",
        "secretname",
        "does not exist",
    ] {
        assert!(
            !seen.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "{needle:?} crossed the wire in the clear"
        );
    }
}

/// A client on a TCP stream, for the checks the guest cannot make.
struct Wire(TcpStream);

impl Wire {
    fn send(&mut self, frame: &[u8]) -> Vec<u8> {
        self.0.write_all(frame).unwrap();
        let s = &mut self.0;
        client::read_frame::<String>(&mut |b| s.read_exact(b).map_err(|e| e.to_string())).unwrap()
    }
}

impl client::Transport for Wire {
    fn exchange(&mut self, frame: &[u8]) -> Result<Vec<u8>, String> {
        Ok(self.send(frame))
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn clear_error(reply: &[u8]) -> Option<String> {
    match Msg::decode(reply).unwrap().frame {
        Frame::Rerror { ename } => Some(ename.to_string()),
        _ => None,
    }
}

#[test]
fn tampered_and_replayed_requests_are_rejected() {
    let s = Scratch::new("replay");
    let l = Lethed::start(
        &["-a", "127.0.0.1:0", "-w", "1", "-k", &test_key()],
        &s.srv(),
    );
    let mut w = Wire(TcpStream::connect(("127.0.0.1", l.port)).unwrap());
    // An unknown key gets no ticket.
    let stranger = Creds {
        user: String::from("glenda"),
        seed: [3; 32],
        cert: None,
    };
    let (f, pending) = client::auth(&stranger, 1, 1, "/", [8; 32]);
    assert_eq!(
        pending.finish(&w.send(&f), now()).err().as_deref(),
        Some("authentication failed")
    );
    let creds = Creds {
        user: String::from("glenda"),
        seed: TEST_SEED,
        cert: None,
    };
    let (f, pending) = client::auth(&creds, 2, 1, "/", [8; 32]);
    let session = pending.finish(&w.send(&f), now()).unwrap();
    let op = Op::Read {
        path: "/hello.txt",
        offset: 0,
        count: 100,
        have: None,
    };
    let (req, p) = session.request(3, now(), &op, [1; 24]);
    match session.reply(&p, &w.send(&req)).unwrap() {
        Owned::Read { data, .. } => assert_eq!(data, HELLO.as_bytes()),
        o => panic!("{o:?}"),
    }
    // Every changed byte is refused in the clear, before the tree is read.
    for i in [4, 5, 10, 20, 40, req.len() / 2, req.len() - 1] {
        let mut bad = req.clone();
        bad[i] ^= 0x20;
        let r = w.send(&bad);
        assert!(clear_error(&r).is_some(), "byte {i}");
    }
    let mut bad = req.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    assert_eq!(
        clear_error(&w.send(&bad)).as_deref(),
        Some("bad record mac")
    );
    // Replayed at once it is a harmless read, answered under a new nonce.
    let again = w.send(&req);
    assert!(clear_error(&again).is_none());
    // Replayed after the window it is refused.
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(
        clear_error(&w.send(&req)).as_deref(),
        Some("request expired")
    );
    // A request made now still goes through on the same ticket.
    let (req, p) = session.request(4, now(), &op, [2; 24]);
    assert!(session.reply(&p, &w.send(&req)).is_ok());
    assert!(l.count("refused: bad record mac") >= 1);
    assert_eq!(l.count("refused: request expired"), 1);
    // The ticket is opaque: nothing of the user's name in it.
    assert!(!session.ticket().windows(6).any(|x| x == b"glenda"));
}
