//! Lethe end to end (`docs/lethe.md`): `lethed` serving a scratch tree on
//! loopback, and the `lethe` command and `#λ` in the emu on both WASM
//! backends reading it. A test client built on `purg_lethe` checks what
//! the guest cannot: tampered and replayed requests. The clear mode
//! (`lethed -A`, `lethe -A`) gets the same checks, the two mismatches, and
//! a relay that shows it is plaintext. Requires `cargo xtask build-root`
//! first.

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
const MANY: usize = 1500;

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
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
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
   1500
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
    // The 1500 entries came in 32 KiB pages.
    assert!(l.count("readdir /many") >= 2 * 3, "{}", l.log());
    // Two runs, each with a ticket for the root, one for /sub and one for
    // /nope, which then is not there.
    assert_eq!(l.count("ticket for"), 6, "{}", l.log());
    assert_eq!(l.count("prefix /sub"), 2);
    // sub/inner was walked in one request.
    assert!(l.count(" walk /") >= 2, "{}", l.log());
}

/// A name in a whole-path walk that is a mount point in the client's
/// namespace ends the walk there, as for `#M`: the rest is walked in what
/// is bound on it, not in the server's tree beneath.
#[test]
fn a_walk_crosses_a_mount_point_in_the_path() {
    let s = Scratch::new("mtpt");
    std::fs::create_dir_all(s.host.join("o")).unwrap();
    std::fs::write(s.host.join("o/inner"), "bound\n").unwrap();
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe -t 0 {addr} {g}/n || exit
bind {g}/o {g}/n/sub
cat {g}/n/sub/inner
unmount {g}/n/sub
cat {g}/n/sub/inner",
        addr = l.addr(),
    );
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert_eq!(out, "bound\ninner file\n", "{vm}");
    }
}

#[test]
fn a_file_changed_underneath_is_not_served_stale() {
    let s = Scratch::new("changed");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    // With -t 0 nothing is trusted: every stat and read asks.
    let script = format!(
        "lethe -t 0 {} {g}/n || exit
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
/// Within its lifetime (`-t`, 1 s by default) what the device learnt is
/// trusted: a file changed underneath is served as it was, without a
/// request, until the lifetime runs out.
#[test]
fn the_cache_lifetime_is_kept() {
    let s = Scratch::new("lifetime");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe -t 600000 {addr} {g}/n || exit
mkdir -p {g}/m && lethe {addr} {g}/m || exit
cat {g}/n/hello.txt {g}/m/hello.txt
echo ready
read < /dev/cons > /dev/null
cat {g}/n/hello.txt
ls -l {g}/n/hello.txt
sleep 2
cat {g}/m/hello.txt",
        addr = l.addr()
    );
    for (i, vm) in VMS.into_iter().enumerate() {
        std::fs::write(s.srv().join("hello.txt"), HELLO).unwrap();
        let changed = format!("changed underneath, {i}\n");
        let mut reads = 0;
        let out = emu_paused(vm, &script, || {
            reads = l.count("read /hello.txt");
            std::fs::write(s.srv().join("hello.txt"), &changed).unwrap();
        });
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(HELLO.trim_end()), "{vm}: {out}");
        assert_eq!(lines.next(), Some(HELLO.trim_end()), "{vm}: {out}");
        assert_eq!(lines.next(), Some("ready"), "{vm}: {out}");
        // The 10-minute mount still has the old data and the old size.
        assert_eq!(lines.next(), Some(HELLO.trim_end()), "{vm}: {out}");
        let ls = lines.next().unwrap_or_default();
        assert!(ls.contains(&format!(" {} ", HELLO.len())), "{vm}: {ls}");
        // The 1-second one asks again after 2.
        assert_eq!(lines.next(), Some(changed.trim_end()), "{vm}: {out}");
        assert_eq!(l.count("read /hello.txt"), reads + 1, "{vm}: {}", l.log());
    }
}

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
            "lethe -t 0 {} {g}/n || exit
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

// The clear mode (`lethed -A`, `lethe -A`): no authentication and no
// encryption, asked for on both ends.

#[test]
fn clear_mode_reads_stats_and_lists() {
    let s = Scratch::new("clear");
    // No key at all: -A alone is enough to start.
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-A"], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe -A {addr} {g}/n || exit
cat {g}/n/hello.txt {g}/n/in
ls {g}/n
ls {g}/n/many | wc -l
ls {g}/n/many/f299
ls -l {g}/n/hello.txt | sed 's/.* ([0-9]+) [A-Z][a-z][a-z] .*/\\1/'
cat {g}/n/sub/inner
cat {g}/n/nope
cat {g}/n/out
ls {g}/n/up
cat {g}/n/hello.txt/x
echo x > {g}/n/hello.txt
cmp {g}/n/big {g}/srv/big && echo same
lethe -A {addr} {g}/n /sub && ls {g}/n
lethe -A {addr} {g}/n /nope",
        addr = l.addr(),
    );
    let want = format!(
        "{HELLO}{HELLO}{g}/n/big
{g}/n/hello.txt
{g}/n/in
{g}/n/many
{g}/n/sub
   1500
{g}/n/many/f299
{len}
inner file
cat: {g}/n/nope: file does not exist
cat: {g}/n/out: file does not exist
ls: {g}/n/up: file does not exist
cat: {g}/n/hello.txt/x: not a directory
sh: {g}/n/hello.txt: permission denied
same
{g}/n/inner
lethe: file does not exist
",
        len = HELLO.len()
    );
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert_eq!(out, want, "{vm}");
    }
    // Everything was answered for `none`, in pages, and nobody got a ticket.
    assert!(l.count("none read /hello.txt") >= 2, "{}", l.log());
    assert!(l.count("none readdir /many") >= 2 * 3, "{}", l.log());
    assert_eq!(l.count("ticket for"), 0, "{}", l.log());
    assert_eq!(l.count("refused"), 0, "{}", l.log());
}

/// Clear requests run as `none`: the other bits, and search on every
/// directory above. The same server still gives the owner a ticket, and
/// through it the owner's bits.
#[test]
fn clear_mode_runs_as_none() {
    use std::os::unix::fs::PermissionsExt;
    let s = Scratch::new("none");
    let srv = s.srv();
    let mode = |p: &Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    };
    std::fs::write(srv.join("private"), "for the owner\n").unwrap();
    std::fs::create_dir(srv.join("closed")).unwrap();
    std::fs::write(srv.join("closed/x"), "behind a closed directory\n").unwrap();
    std::fs::create_dir(srv.join("unlisted")).unwrap();
    std::fs::write(srv.join("unlisted/x"), "in an unlisted directory\n").unwrap();
    mode(&srv.join("hello.txt"), 0o644);
    mode(&srv.join("private"), 0o640);
    mode(&srv.join("closed/x"), 0o644);
    mode(&srv.join("closed"), 0o750);
    mode(&srv.join("unlisted/x"), 0o644);
    mode(&srv.join("unlisted"), 0o711);
    // -u none: even a tree whose owner is called none gives a clear
    // request only the other bits.
    for owner in ["glenda", "none"] {
        let l = Lethed::start(
            &["-a", "127.0.0.1:0", "-A", "-u", owner, "-k", &test_key()],
            &srv,
        );
        let g = &s.guest;
        let a = l.addr();
        let script = format!(
            "lethe -A {a} {g}/n || exit
cat {g}/n/hello.txt
cat {g}/n/private
cat {g}/n/closed/x
ls {g}/n/closed
cat {g}/n/unlisted/x
ls {g}/n/unlisted
lethe -A {a} {g}/n /closed
ls {g}/n
lethe -u {owner} {a} {g}/n || exit
cat {g}/n/private {g}/n/closed/x
ls {g}/n/unlisted"
        );
        let want = format!(
            "{HELLO}cat: {g}/n/private: permission denied
cat: {g}/n/closed/x: permission denied
ls: {g}/n/closed: permission denied
in an unlisted directory
ls: {g}/n/unlisted: permission denied
ls: {g}/n: permission denied
for the owner
behind a closed directory
{g}/n/unlisted/x
"
        );
        for vm in VMS {
            let (out, _) = emu(vm, &script);
            assert_eq!(out, want, "{vm}, owner {owner}");
        }
        assert!(
            l.count("none read /private: permission denied") >= 2,
            "{}",
            l.log()
        );
        assert!(l.count(&format!("ticket for {owner}")) >= 2, "{}", l.log());
    }
    // Let the scratch tree be removed.
    mode(&srv.join("closed"), 0o755);
}

/// Neither end falls back to the other mode: each mismatch is an error.
#[test]
fn clear_and_sealed_modes_do_not_mix() {
    let s = Scratch::new("mismatch");
    let sealed = Lethed::start(&["-a", "127.0.0.1:0", "-k", &test_key()], &s.srv());
    let clear = Lethed::start(&["-a", "127.0.0.1:0", "-A"], &s.srv());
    let g = &s.guest;
    let script = format!(
        "lethe -A {sealed} {g}/n
ls {g}/n
lethe {clear} {g}/n
ls {g}/n
lethe -A -k {g}/key {clear} {g}/n
lethe -A -u glenda {clear} {g}/n
echo 'dial {clear}
noauth
user glenda' > '#λ/clone'
echo end",
        sealed = sealed.addr(),
        clear = clear.addr(),
    );
    let want = "lethe: authentication required
lethe: authentication not required
lethe: usage: lethe [-abc] [-A] [-k keyfile] [-t ms] [-u user] addr mtpt [path]
lethe: usage: lethe [-abc] [-A] [-k keyfile] [-t ms] [-u user] addr mtpt [path]
echo: bad arg in system call
end
";
    for vm in VMS {
        let (out, _) = emu(vm, &script);
        assert_eq!(out, want, "{vm}");
    }
    // The sealed server read nothing for the clear client, and the clear
    // server gave the sealed client no ticket and no data.
    assert_eq!(
        sealed.count("refused: authentication required"),
        2,
        "{}",
        sealed.log()
    );
    assert_eq!(sealed.count("none "), 0, "{}", sealed.log());
    assert_eq!(
        clear.count("refused: authentication not required"),
        2,
        "{}",
        clear.log()
    );
    assert_eq!(clear.count("none "), 0, "{}", clear.log());
    // The same from a client on the wire: a clear request is refused in
    // an outer Rerror, and a sealed-only server never sends an Rclear.
    let mut w = Wire(TcpStream::connect(("127.0.0.1", sealed.port)).unwrap());
    let op = Op::Stat { path: "/hello.txt" };
    let (f, p) = client::clear(1, &op);
    let reply = w.send(&f);
    assert_eq!(
        clear_error(&reply).as_deref(),
        Some("authentication required")
    );
    assert_eq!(
        p.reply(&reply),
        Err(client::Failure::Clear(String::from(
            "authentication required"
        )))
    );
    let mut w = Wire(TcpStream::connect(("127.0.0.1", clear.port)).unwrap());
    let (f, p) = client::clear(2, &op);
    match p.reply(&w.send(&f)).unwrap() {
        Owned::Stat(d) => assert_eq!(d.name, "hello.txt"),
        o => panic!("{o:?}"),
    }
    let creds = Creds {
        user: String::from("glenda"),
        seed: TEST_SEED,
        cert: None,
    };
    let (f, pending) = client::auth(&creds, 3, 1, "/", [8; 32]);
    assert_eq!(
        pending.finish(&w.send(&f), now()).err().as_deref(),
        Some("authentication not required")
    );
}

/// Without -A and without a key to trust, lethed does not start.
#[test]
fn lethed_fails_closed() {
    let s = Scratch::new("closed");
    let o = Command::new(env!("CARGO_BIN_EXE_lethed"))
        .args(["-a", "127.0.0.1:0"])
        .arg(s.srv())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(o.stdout.is_empty());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("no client can authenticate"), "{err}");
}

fn crossed(seen: &[u8], needle: &str) -> bool {
    seen.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The clear mode really is plaintext, and on the same server (-A with a
/// trusted key) the sealed mode still is not.
#[test]
fn the_clear_wire_is_plaintext_and_the_sealed_one_is_not() {
    let s = Scratch::new("clearwire");
    let l = Lethed::start(&["-a", "127.0.0.1:0", "-A", "-k", &test_key()], &s.srv());
    let g = &s.guest;
    let needles = [
        "hello from lethe",
        "hello.txt",
        "inner",
        "secretname",
        "does not exist",
    ];
    for (flag, clear) in [("-A ", true), ("", false)] {
        let (port, seen) = relay(l.port);
        let script = format!(
            "lethe {flag}tcp!127.0.0.1!{port} {g}/n || exit
cat {g}/n/hello.txt
ls {g}/n/sub
cat {g}/n/secretname"
        );
        for vm in VMS {
            let (out, _) = emu(vm, &script);
            assert!(out.starts_with(HELLO), "{vm}: {out}");
        }
        let seen = seen.lock().unwrap();
        assert!(seen.len() > 500, "{}", seen.len());
        for needle in needles {
            assert_eq!(
                crossed(&seen, needle),
                clear,
                "{needle:?} with {flag:?}: clear {clear}"
            );
        }
    }
    assert!(l.count("none read /hello.txt") >= 2, "{}", l.log());
    assert!(l.count("ticket for") >= 2, "{}", l.log());
}

/// The cache, the version check and the redial work the same in the
/// clear: a file changed underneath is read fresh, and a server restarted
/// in the middle of a read costs the reader nothing.
#[test]
fn clear_mode_sees_changes_and_survives_a_restart() {
    let s = Scratch::new("clearrestart");
    let g = &s.guest;
    for (i, vm) in VMS.into_iter().enumerate() {
        std::fs::write(s.srv().join("hello.txt"), HELLO).unwrap();
        let first = Lethed::start(&["-a", "127.0.0.1:0", "-A"], &s.srv());
        let addr = format!("127.0.0.1:{}", first.port);
        let script = format!(
            "lethe -A -t 0 {} {g}/n || exit
cat {g}/n/hello.txt
cat {g}/n/hello.txt
{{read 8192 > {g}/part1; echo ready; read < /dev/cons > /dev/null; cat > {g}/part2}} < {g}/n/big
cat {g}/n/hello.txt
echo done",
            first.addr()
        );
        let changed = format!("changed underneath, {i}\n");
        let mut second = None;
        let out = emu_paused(vm, &script, || {
            std::fs::write(s.srv().join("hello.txt"), &changed).unwrap();
            // The second read of hello.txt was conditional.
            assert!(first.count("none read /hello.txt") >= 2, "{}", first.log());
            drop(first);
            second = Some(Lethed::start(&["-a", &addr, "-A"], &s.srv()));
        });
        assert_eq!(out, format!("{HELLO}{HELLO}ready\n{changed}done\n"), "{vm}");
        let big = std::fs::read(s.srv().join("big")).unwrap();
        let mut got = std::fs::read(s.host.join("part1")).unwrap();
        assert_eq!(got.len(), 8192, "{vm}");
        got.extend(std::fs::read(s.host.join("part2")).unwrap());
        assert!(got == big, "{vm}: the read across the restart differs");
        let second = second.unwrap();
        assert!(second.count("none read /big") > 0, "{}", second.log());
        assert_eq!(second.count("ticket for"), 0, "{}", second.log());
    }
}
