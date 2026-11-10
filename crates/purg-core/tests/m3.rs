//! Mount, export, file2chan and kill.

use std::sync::Arc;
use std::thread;

use purg_abi::mode;
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::kchan::KMsg;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;

#[derive(Default)]
struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock {
    console: MockConsole,
}

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        0
    }
    fn monotonic_ns(&self) -> u64 {
        0
    }
    fn entropy(&self, buf: &mut [u8]) {
        buf.fill(1);
    }
    fn console(&self) -> &dyn Console {
        &self.console
    }
    fn user(&self) -> String {
        "inferno".into()
    }
    fn sysname(&self) -> String {
        "purgatorio".into()
    }
    fn current_pid(&self) -> u32 {
        PID.with(|c| c.get())
    }
    fn set_current_pid(&self, pid: u32) {
        PID.with(|c| c.set(pid));
    }
    fn spawn_kproc(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(drop)
            .map_err(|_| purg_abi::error::ENOPROC.into())
    }
}

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock {
        console: MockConsole,
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let ram = RamTree::new('R', "ram", "inferno", platform.clone());
    ram.add_dir("/mnt");
    ram.add_dir("/box");
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    mock.set_current_pid(p.pid);
    purg_platform::sync::set_current_note(Some(p.proc.note.clone()));
    (p, mock)
}

#[test]
fn namespace_lists_binds() {
    let (p, _) = setup();
    p.bind("#c", "/dev", purg_abi::mount::MBEFORE).unwrap();
    p.bind(
        "#R",
        "/",
        purg_abi::mount::MAFTER | purg_abi::mount::MCREATE,
    )
    .unwrap();
    let ns = p.pgrp.namespace();
    assert!(ns.contains("bind -b '#c' /dev\n"), "{ns}");
    assert!(ns.contains("bind -ac '#R' /\n"), "{ns}");
    assert!(ns.ends_with("cd /\n"), "{ns}");
}

#[test]
fn file2chan_serves_a_read() {
    let (mut p, _) = setup();
    let (rh, _wh) = p.file2chan("#R/box", "g").unwrap();
    let kgrp = p.proc.kgrp.clone();
    let read_ch = kgrp.get(rh).unwrap();
    let server = thread::spawn(move || {
        let msg = read_ch.recv().unwrap().expect("request");
        let reply = i32::from_le_bytes(msg.bytes[17..21].try_into().unwrap());
        let reply_ch = kgrp.get(reply).unwrap();
        let mut bytes = vec![purg_abi::fio::OK_READ];
        bytes.extend_from_slice(b"xyz");
        reply_ch
            .send(KMsg {
                bytes,
                caps: Vec::new(),
            })
            .unwrap();
    });
    let fd = p.open("#R/box/g", mode::OREAD).unwrap();
    let mut buf = [0u8; 8];
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"xyz");
    server.join().unwrap();
}

#[cfg(feature = "std")]
#[test]
fn pipe_between_progs() {
    let (mut p, _) = setup();
    let [a, b] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(a, 0)]).unwrap();
    child.started();
    p.close(a).unwrap();
    let t = thread::spawn(move || {
        let mut buf = [0u8; 8];
        let n = child.read(0, &mut buf).unwrap();
        buf[..n].to_vec()
    });
    assert_eq!(p.write(b, b"hello").unwrap(), 5);
    assert_eq!(t.join().unwrap(), b"hello");
}

#[cfg(feature = "std")]
#[test]
fn export_mount_loopback() {
    let (mut p, mock) = setup();
    let [a, b] = p.pipe().unwrap();
    let [ready_r, ready_w] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(a, 0), (ready_w, 1)]).unwrap();
    child.started();
    p.close(a).unwrap();
    p.close(ready_w).unwrap();
    let mock = mock.clone();
    let server = thread::spawn(move || {
        mock.set_current_pid(child.pid);
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        child.write(1, b"Z").unwrap();
        child.export(0, "#R", 0).unwrap();
    });
    let mut flag = [0u8; 1];
    assert_eq!(p.read(ready_r, &mut flag).unwrap(), 1);
    p.mount(
        b,
        -1,
        "#R/mnt",
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
        "",
    )
    .unwrap();
    p.close(b).unwrap();
    let fd = p.create("#R/mnt/hello", mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, b"hi").unwrap(), 2);
    p.close(fd).unwrap();
    let mut buf = [0u8; 8];
    let fd = p.open("#R/hello", mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
    p.close(fd).unwrap();
    let fd = p.open("#R/mnt/hello", mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
    p.close(fd).unwrap();
    let ns = p.pgrp.namespace();
    assert!(ns.contains("mount ") && ns.contains("#R/mnt"), "{ns}");
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

#[test]
fn factotum_signs_a_challenge() {
    let (mut p, _) = setup();
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let fd = p.open("#¤/ctl", mode::OWRITE).unwrap();
    assert_eq!(p.write(fd, &seed).unwrap(), seed.len());
    p.close(fd).unwrap();
    let msg = b"challenge";
    let fd = p.open("#¤/rpc", mode::ORDWR).unwrap();
    assert_eq!(p.write(fd, msg).unwrap(), msg.len());
    p.seek(fd, 0, 0).unwrap();
    let mut buf = [0u8; purg_core::auth::PROOF];
    assert_eq!(p.read(fd, &mut buf).unwrap(), buf.len());
    let public = purg_core::auth::public_key(&seed);
    let signature: [u8; purg_core::auth::SIGNATURE] =
        buf[..purg_core::auth::SIGNATURE].try_into().unwrap();
    assert!(purg_core::auth::verify(&public, msg, &signature));
    assert_eq!(&buf[purg_core::auth::SIGNATURE..], &public);
}

#[cfg(feature = "std")]
fn install_fixture(p: &mut Prog) {
    p.bind(
        "#R",
        "/",
        purg_abi::mount::MAFTER | purg_abi::mount::MCREATE,
    )
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

#[cfg(feature = "std")]
fn spawn_export(p: &mut Prog, mock: Arc<Mock>, root: &str) -> (thread::JoinHandle<()>, i32) {
    let root = String::from(root);
    let [a, b] = p.pipe().unwrap();
    let [ready_r, ready_w] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(a, 0), (ready_w, 1)]).unwrap();
    child.started();
    p.close(a).unwrap();
    p.close(ready_w).unwrap();
    let server = thread::spawn(move || {
        mock.set_current_pid(child.pid);
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        child.write(1, b"Z").unwrap();
        child.export(0, &root, 0).unwrap();
    });
    let mut flag = [0u8; 1];
    assert_eq!(p.read(ready_r, &mut flag).unwrap(), 1);
    p.close(ready_r).unwrap();
    (server, b)
}

#[cfg(feature = "std")]
#[test]
fn authenticated_mount_reads_a_file() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, mock, "#R");
    p.mount(
        b,
        -1,
        "#R/mnt",
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
        "",
    )
    .unwrap();
    p.close(b).unwrap();
    let fd = p.create("#R/mnt/hello", mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, b"hi").unwrap(), 2);
    p.close(fd).unwrap();
    let mut buf = [0u8; 8];
    let fd = p.open("#R/hello", mode::OREAD).unwrap();
    let n = p.read(fd, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
    p.close(fd).unwrap();
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

#[cfg(feature = "std")]
#[test]
fn factotum_mount_without_a_key_file() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    p.remove("/lib/test/auth.key").unwrap();
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let fd = p.open("#¤/ctl", mode::OWRITE).unwrap();
    assert_eq!(p.write(fd, &seed).unwrap(), seed.len());
    p.close(fd).unwrap();
    let (server, b) = spawn_export(&mut p, mock, "#R");
    p.mount(
        b,
        -1,
        "#R/mnt",
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
        "",
    )
    .unwrap();
    p.close(b).unwrap();
    let fd = p.create("#R/mnt/hello", mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, b"hi").unwrap(), 2);
    p.close(fd).unwrap();
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

#[cfg(feature = "std")]
#[test]
fn unauthenticated_mount_is_rejected() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, mock, "#R");
    let err = p
        .mount(
            b,
            -1,
            "#R/mnt",
            purg_abi::mount::MREPL | purg_abi::mount::MNOAUTH,
            "",
        )
        .unwrap_err();
    assert_eq!(err.as_str(), purg_core::auth::REQUIRED);
    p.close(b).unwrap();
    server.join().unwrap();
}

/// Hosted only: without the `std` feature the platform has no per-thread
/// note, so this is the emu's thread-per-prog model (`cargo test -p purg-core
/// --features std`).
#[cfg(feature = "std")]
#[test]
fn kill_interrupts_a_blocked_read() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (mut p, _) = setup();
    let [r, w] = p.pipe().unwrap();
    let mut child = p.spawn(0, &[(r, 0)]).unwrap();
    child.started();
    let pid = child.pid;
    let ready = Arc::new(AtomicBool::new(false));
    let flag = ready.clone();
    let join = thread::spawn(move || {
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        flag.store(true, Ordering::Release);
        let mut buf = [0u8; 4];
        let err = child.read(0, &mut buf).unwrap_err();
        assert_eq!(err.as_str(), purg_abi::error::EINTR);
    });
    while !ready.load(Ordering::Acquire) {
        thread::yield_now();
    }
    p.kernel.procs.get(pid).unwrap().kill();
    join.join().unwrap();
    let _ = (w, p);
}

/// fauth, the exchange done by the caller on the auth fd, then a mount
/// that attaches with it.
#[cfg(feature = "std")]
#[test]
fn fauth_then_mount_with_the_afd() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, mock, "#R");
    let afd = p.fauth(b, "").unwrap();
    let mut challenge = [0u8; purg_core::auth::CHALLENGE];
    assert_eq!(p.read(afd, &mut challenge).unwrap(), challenge.len());
    let seed = [purg_core::auth::FIXTURE; purg_core::auth::SEED];
    let mut proof = purg_core::auth::sign(&seed, &challenge).to_vec();
    proof.extend_from_slice(&purg_core::auth::public_key(&seed));
    assert_eq!(p.write(afd, &proof).unwrap(), proof.len());
    p.mount(
        b,
        afd,
        "#R/mnt",
        purg_abi::mount::MREPL | purg_abi::mount::MCREATE,
        "",
    )
    .unwrap();
    p.close(afd).unwrap();
    p.close(b).unwrap();
    let mut buf = [0u8; 64];
    let fd = p.open("#R/mnt/lib/test/auth.key.pub", mode::OREAD).unwrap();
    assert_eq!(p.read(fd, &mut buf).unwrap(), purg_core::auth::PUBLIC);
    p.close(fd).unwrap();
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

/// An afd that never authenticated does not get the attach in, and a
/// server without a key answers fauth with "authentication not required".
#[cfg(feature = "std")]
#[test]
fn fauth_failures() {
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    let (server, b) = spawn_export(&mut p, mock.clone(), "#R");
    let afd = p.fauth(b, "").unwrap();
    let err = p.mount(b, afd, "#R/mnt", 0, "").unwrap_err();
    assert_eq!(err.as_str(), purg_core::auth::REQUIRED);
    let err = p.mount(afd, afd, "#R/mnt", 0, "").unwrap_err();
    assert_eq!(err.as_str(), purg_abi::error::EBADUSEFD);
    p.close(afd).unwrap();
    p.close(b).unwrap();
    server.join().unwrap();

    let (server, b) = spawn_export(&mut p, mock, "#R/box");
    let err = p.fauth(b, "").unwrap_err();
    assert_eq!(err.as_str(), purg_core::auth::NOT_REQUIRED);
    p.mount(b, -1, "#R/mnt", purg_abi::mount::MREPL, "")
        .unwrap();
    p.close(b).unwrap();
    p.unmount(None, "#R/mnt").unwrap();
    server.join().unwrap();
}

#[test]
fn stream_copies_through_a_small_buffer() {
    let (mut p, _) = setup();
    let data = b"stream through a seven byte buffer";
    let [r, w] = p.pipe().unwrap();
    assert_eq!(p.write(w, data).unwrap(), data.len());
    p.close(w).unwrap();
    let out = p.create("#R/box/out", mode::ORDWR, 0o644).unwrap();
    let (n, failed) = p.stream(r, out, 7).unwrap();
    assert_eq!(n, data.len() as u64);
    assert!(failed.is_none());
    let mut buf = [0u8; 64];
    let n = p.pread(out, &mut buf, 0).unwrap();
    assert_eq!(&buf[..n], data);

    // A failed write ends the copy and that buffer is not counted.
    let src = p.open("#R/box/out", mode::OREAD).unwrap();
    let (n, failed) = p.stream(src, src, 4).unwrap();
    assert_eq!(n, 0);
    assert_eq!(failed.unwrap().as_str(), purg_abi::error::EBADUSEFD);
    assert!(p.stream(r, out, -1).is_err());
}

/// A prog blocked reading in stream is interrupted by kill, writes out what
/// it holds, and returns the count.
#[cfg(feature = "std")]
#[test]
fn kill_interrupts_a_blocked_stream() {
    let (mut p, _) = setup();
    let [r, w] = p.pipe().unwrap();
    let out = p.create("#R/box/killed", mode::ORDWR, 0o644).unwrap();
    let mut child = p.spawn(0, &[(r, 0), (out, 1)]).unwrap();
    child.started();
    let pid = child.pid;
    let join = thread::spawn(move || {
        purg_platform::sync::set_current_note(Some(child.proc.note.clone()));
        child.stream(0, 1, 4).unwrap()
    });
    assert_eq!(p.write(w, b"full").unwrap(), 4);
    // A full buffer goes straight to the file. Once it is there, the child
    // is in (or about to enter) its next read.
    let mut buf = [0u8; 8];
    while p.pread(out, &mut buf, 0).unwrap() < 4 {
        thread::yield_now();
    }
    p.kernel.procs.get(pid).unwrap().kill();
    let (n, failed) = join.join().unwrap();
    assert_eq!(n, 4);
    assert_eq!(failed.unwrap().as_str(), purg_abi::error::EINTR);
    assert_eq!(&buf[..4], b"full");
    let _ = w;
}

fn write_file(p: &mut Prog, path: &str, bytes: &[u8]) {
    let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, bytes).unwrap(), bytes.len());
    p.close(fd).unwrap();
}

/// A server that trusts a signer takes a certificate from the client's
/// authinfo or from `#¤`, and refuses it once the key is revoked.
#[cfg(feature = "std")]
#[test]
fn certificate_mount() {
    use purg_auth::cert::Cert;
    use purg_auth::keys::{AuthInfo, list_line};
    let (mut p, mock) = setup();
    install_fixture(&mut p);
    p.remove("/lib/test/auth.key").unwrap();
    let signer = [5u8; 32];
    let user = [6u8; 32];
    let mut cert = Cert::new("inferno", purg_auth::public_key(&user), 0).unwrap();
    cert.sign(&signer).unwrap();
    for dir in [
        "/box/lib",
        "/box/lib/keys",
        "/usr",
        "/usr/inferno",
        "/usr/inferno/keyring",
    ] {
        let fd = p
            .create(&format!("#R{dir}"), mode::OREAD, purg_styx::DMDIR)
            .unwrap();
        p.close(fd).unwrap();
    }
    let signers = list_line(&purg_auth::public_key(&signer), "test");
    write_file(&mut p, "#R/box/lib/keys/signers", signers.as_bytes());
    let info = AuthInfo {
        seed: user,
        signer: purg_auth::public_key(&signer),
        cert: cert.clone(),
    };
    write_file(
        &mut p,
        "/usr/inferno/keyring/default",
        info.encode().as_bytes(),
    );

    let mount = |p: &mut Prog, mock: Arc<Mock>| {
        let (server, b) = spawn_export(p, mock, "#R/box");
        let r = p.mount(b, -1, "#R/mnt", purg_abi::mount::MREPL, "");
        p.close(b).unwrap();
        if r.is_ok() {
            p.unmount(None, "#R/mnt").unwrap();
        }
        server.join().unwrap();
        r.map_err(|e| String::from(e.as_str()))
    };
    assert_eq!(mount(&mut p, mock.clone()), Ok(()));

    let revoked = list_line(&purg_auth::public_key(&user), "");
    write_file(&mut p, "#R/box/lib/keys/revoked", revoked.as_bytes());
    assert_eq!(
        mount(&mut p, mock.clone()),
        Err(String::from(purg_core::auth::REVOKED))
    );
    p.remove("#R/box/lib/keys/revoked").unwrap();

    // The same key and certificate held by #¤ instead of a file.
    p.remove("/usr/inferno/keyring/default").unwrap();
    assert_eq!(
        mount(&mut p, mock.clone()),
        Err(String::from(purg_core::auth::NO_KEY))
    );
    let fd = p.open("#¤/ctl", mode::OWRITE).unwrap();
    p.write(fd, &user).unwrap();
    let line = format!("cert {}", cert.encode());
    p.write(fd, line.as_bytes()).unwrap();
    p.close(fd).unwrap();
    assert_eq!(mount(&mut p, mock.clone()), Ok(()));

    // Without the certificate, #¤'s key alone is not trusted.
    let fd = p.open("#¤/ctl", mode::OWRITE).unwrap();
    p.write(fd, &user).unwrap();
    p.close(fd).unwrap();
    assert_eq!(
        mount(&mut p, mock),
        Err(String::from(purg_core::auth::FAILED))
    );
}
