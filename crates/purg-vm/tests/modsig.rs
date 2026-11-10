//! The loader's module policy (`docs/signing.md`): each level, each way
//! a module fails, how the level is set and inherited, and where the
//! trust lists are read.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use purg_abi::mode;
use purg_abi::mount::{MAFTER, MCREATE, MREPL};
use purg_auth::cert::Cert;
use purg_auth::keys::list_line;
use purg_auth::modsig;
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::modsig::Level;
use purg_core::{Dev, DevTab, Error, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_vm::{Ctx, Engine, Stop, load_module};

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
    now: AtomicU64,
}

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Relaxed) * 1_000_000_000
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
    fn spawn_kproc(&self, _name: &str, _f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        panic!("no test here starts a kproc");
    }
}

/// An engine for [`purg_vm::spawn`] calls that must fail before running.
struct NoEngine;

impl Engine for NoEngine {
    fn name(&self) -> &'static str {
        "none"
    }
    fn run(&self, _wasm: Vec<u8>, ctx: Ctx) -> (Ctx, Result<Stop, Error>) {
        (ctx, Err(Error::new("not run")))
    }
}

const SIGNER: [u8; 32] = [0x42; 32];
const OTHER: [u8; 32] = [7; 32];
const ROOT: [u8; 32] = [9; 32];

fn public(seed: &[u8; 32]) -> [u8; 32] {
    purg_auth::public_key(seed)
}

/// A first prog whose namespace has a writable root with `/lib/keys`,
/// `/bin` and `/tmp`, and whose fds 0 to 2 are the file `/tmp/cons`.
fn setup() -> (Prog, Arc<Mock>) {
    let mock = Arc::new(Mock {
        console: MockConsole,
        now: AtomicU64::new(1000),
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(Arc::new(RamTree::new(
        'R',
        "ram",
        "inferno",
        platform.clone(),
    )));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    pgrp.bind("#R", "/", MAFTER | MCREATE).unwrap();
    let mut p = Prog::new(kernel, pgrp);
    mock.set_current_pid(p.pid);
    purg_platform::sync::set_current_note(Some(p.proc.note.clone()));
    for dir in ["/lib", "/lib/keys", "/bin", "/tmp", "/fake"] {
        let fd = p.create(dir, mode::OREAD, purg_styx::DMDIR).unwrap();
        p.close(fd).unwrap();
    }
    write_file(&mut p, "/tmp/cons", b"");
    for _ in 0..3 {
        p.open("/tmp/cons", mode::ORDWR).unwrap();
    }
    (p, mock)
}

fn write_file(p: &mut Prog, path: &str, bytes: &[u8]) {
    let _ = p.remove(path);
    let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
    assert_eq!(p.write(fd, bytes).unwrap(), bytes.len());
    p.close(fd).unwrap();
}

fn read_file(p: &mut Prog, path: &str) -> String {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = p.read(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(fd).unwrap();
    String::from_utf8(out).unwrap()
}

/// What the prog has written to its fd 2 since the last call.
fn warnings(p: &mut Prog) -> String {
    let text = read_file(p, "/tmp/cons");
    let fd = p.create("/tmp/cons", mode::OWRITE | mode::OTRUNC, 0o644);
    if let Ok(fd) = fd {
        p.close(fd).unwrap();
    }
    p.seek(2, 0, 0).unwrap();
    text
}

/// A module: the header, the ABI section and a section holding `tag`.
/// The loader does not look inside sections, so this is enough for it.
fn module(tag: &[u8]) -> Vec<u8> {
    let mut m = b"\0asm\x01\0\0\0".to_vec();
    let abi = purg_abi::ABI_VERSION.as_bytes();
    let name = purg_abi::ABI_SECTION.as_bytes();
    m.push(0);
    m.push((1 + name.len() + abi.len()) as u8);
    m.push(name.len() as u8);
    m.extend_from_slice(name);
    m.extend_from_slice(abi);
    m.push(11);
    m.push(tag.len() as u8);
    m.extend_from_slice(tag);
    m
}

fn signed(tag: &[u8], seed: &[u8; 32]) -> Vec<u8> {
    modsig::sign(&module(tag), seed, Vec::new()).unwrap()
}

fn set_level(p: &mut Prog, pid: u32, level: &str) -> Result<(), String> {
    let fd = p
        .open(&format!("#p/{pid}/ctl"), mode::OWRITE)
        .map_err(|e| String::from(e.as_str()))?;
    let r = p.write(fd, format!("modules {level}").as_bytes());
    p.close(fd).unwrap();
    r.map(drop).map_err(|e| String::from(e.as_str()))
}

fn load(p: &mut Prog, path: &str, wasm: &[u8]) -> Result<(), String> {
    write_file(p, path, wasm);
    load_module(p, path)
        .map(|loaded| assert_eq!(loaded, wasm))
        .map_err(|e| String::from(e.as_str()))
}

fn trust(p: &mut Prog, seeds: &[&[u8; 32]]) {
    let text: String = seeds
        .iter()
        .map(|s| list_line(&public(s), "test"))
        .collect();
    write_file(p, "/lib/keys/modules", text.as_bytes());
}

fn hex(b: &[u8]) -> String {
    purg_auth::hex::encode(b)
}

#[test]
fn none_checks_nothing_and_records_the_digest() {
    let (mut p, _) = setup();
    // Lists that would refuse everything are not even read.
    write_file(&mut p, "/lib/keys/revoked", b"damaged\n");
    let plain = module(b"one");
    let sig = signed(b"two", &SIGNER);
    assert_eq!(load(&mut p, "/bin/one.wasm", &plain), Ok(()));
    assert_eq!(load(&mut p, "/bin/two.wasm", &sig), Ok(()));
    let mut broken = sig.clone();
    let at = broken.len() - 40;
    broken[at] ^= 1;
    assert_eq!(load(&mut p, "/bin/my lib.wasm", &broken), Ok(()));
    assert_eq!(warnings(&mut p), "");
    let pid = p.pid;
    assert_eq!(
        read_file(&mut p, &format!("#p/{pid}/module")),
        format!(
            "policy none\nmodule {} - /bin/one.wasm\nmodule {} - /bin/two.wasm\n\
             module {} - '/bin/my lib.wasm'\n",
            hex(&modsig::digest(&plain).unwrap()),
            hex(&modsig::digest(&module(b"two")).unwrap()),
            hex(&modsig::digest(&module(b"two")).unwrap()),
        )
    );
    // What is not a module is still refused.
    assert_eq!(
        load(&mut p, "/bin/x.wasm", b"#!/bin/sh\n"),
        Err(String::from(purg_abi::error::EBADEXEC))
    );
    let mut tail = plain.clone();
    tail.push(0);
    assert_eq!(
        load(&mut p, "/bin/x.wasm", &tail),
        Err(String::from(purg_abi::error::EBADEXEC))
    );
}

#[test]
fn warn_loads_and_says_why() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    let pid = p.pid;
    set_level(&mut p, pid, "warn").unwrap();
    assert_eq!(load(&mut p, "/bin/plain.wasm", &module(b"a")), Ok(()));
    assert_eq!(
        warnings(&mut p),
        "/bin/plain.wasm: warning: module not signed\n"
    );
    assert_eq!(
        load(&mut p, "/bin/good.wasm", &signed(b"b", &SIGNER)),
        Ok(())
    );
    assert_eq!(warnings(&mut p), "");
    assert_eq!(
        load(&mut p, "/bin/other.wasm", &signed(b"c", &OTHER)),
        Ok(())
    );
    assert_eq!(
        warnings(&mut p),
        "/bin/other.wasm: warning: untrusted signer\n"
    );
    let mut broken = signed(b"d", &SIGNER);
    let at = broken.len() - 40;
    broken[at] ^= 1;
    assert_eq!(load(&mut p, "/bin/broken.wasm", &broken), Ok(()));
    assert_eq!(
        warnings(&mut p),
        "/bin/broken.wasm: warning: bad signature\n"
    );
    let text = read_file(&mut p, &format!("#p/{pid}/module"));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "policy warn");
    assert!(lines[1].ends_with(" - /bin/plain.wasm"), "{text}");
    assert_eq!(
        lines[2],
        format!(
            "module {} {} /bin/good.wasm",
            hex(&modsig::digest(&module(b"b")).unwrap()),
            hex(&public(&SIGNER))
        )
    );
    assert!(lines[3].ends_with(" - /bin/other.wasm"), "{text}");
    assert!(lines[4].ends_with(" - /bin/broken.wasm"), "{text}");
}

#[test]
fn require_refuses_each_failure() {
    let (mut p, mock) = setup();
    trust(&mut p, &[&SIGNER, &ROOT]);
    let pid = p.pid;
    set_level(&mut p, pid, "require").unwrap();
    let err = |e: &str| Err(String::from(e));

    let good = signed(b"good", &SIGNER);
    assert_eq!(load(&mut p, "/bin/good.wasm", &good), Ok(()));
    assert_eq!(
        load(&mut p, "/bin/plain.wasm", &module(b"p")),
        err(modsig::NOT_SIGNED)
    );
    assert_eq!(
        load(&mut p, "/bin/other.wasm", &signed(b"o", &OTHER)),
        err(modsig::UNTRUSTED)
    );

    // Changed after signing: a byte of the code, an added section, a
    // second signature section, a mangled section.
    let unsigned_len = module(b"good").len();
    let mut changed = good.clone();
    changed[unsigned_len - 1] ^= 1;
    assert_eq!(
        load(&mut p, "/bin/t.wasm", &changed),
        err(modsig::BAD_SIGNATURE)
    );
    let mut added = good.clone();
    added.extend_from_slice(&[0, 2, 1, b'x']);
    assert_eq!(
        load(&mut p, "/bin/t.wasm", &added),
        err(modsig::BAD_SIGNATURE)
    );
    let mut twice = good.clone();
    twice.extend_from_slice(&good[unsigned_len..]);
    assert_eq!(
        load(&mut p, "/bin/t.wasm", &twice),
        err(modsig::BAD_SIGNATURE)
    );
    let mut mangled = good.clone();
    mangled[unsigned_len + 17] = 9;
    assert_eq!(
        load(&mut p, "/bin/t.wasm", &mangled),
        err(modsig::BAD_SIGNATURE)
    );
    let mut cut = good.clone();
    cut.truncate(good.len() - 1);
    assert_eq!(
        load(&mut p, "/bin/t.wasm", &cut),
        err(purg_abi::error::EBADEXEC)
    );

    // Two signers: one trusted is enough.
    let both = modsig::sign(&signed(b"both", &OTHER), &SIGNER, Vec::new()).unwrap();
    assert_eq!(load(&mut p, "/bin/both.wasm", &both), Ok(()));

    // A certificate chain to a trusted key, and its expiry.
    let mut cert = Cert::new("builder", public(&OTHER), 2000).unwrap();
    cert.sign(&ROOT).unwrap();
    let chained = modsig::sign(&module(b"chain"), &OTHER, vec![cert]).unwrap();
    assert_eq!(load(&mut p, "/bin/chain.wasm", &chained), Ok(()));
    mock.now.store(2000, Ordering::Relaxed);
    assert_eq!(
        load(&mut p, "/bin/chain.wasm", &chained),
        err(modsig::EXPIRED)
    );
    mock.now.store(1000, Ordering::Relaxed);

    // Revocation: the certifying key, the signing key, the module itself,
    // and a list that does not parse.
    let revoke = |p: &mut Prog, key: &[u8; 32]| {
        write_file(p, "/lib/keys/revoked", list_line(key, "").as_bytes());
    };
    revoke(&mut p, &public(&ROOT));
    assert_eq!(
        load(&mut p, "/bin/chain.wasm", &chained),
        err(modsig::REVOKED)
    );
    assert_eq!(load(&mut p, "/bin/good.wasm", &good), Ok(()));
    revoke(&mut p, &public(&SIGNER));
    assert_eq!(load(&mut p, "/bin/good.wasm", &good), err(modsig::REVOKED));
    assert_eq!(load(&mut p, "/bin/both.wasm", &both), err(modsig::REVOKED));
    revoke(&mut p, &modsig::digest(&good).unwrap());
    assert_eq!(
        load(&mut p, "/bin/good.wasm", &good),
        err(modsig::MODULE_REVOKED)
    );
    assert_eq!(load(&mut p, "/bin/both.wasm", &both), Ok(()));
    write_file(&mut p, "/lib/keys/revoked", b"not a key\n");
    assert_eq!(load(&mut p, "/bin/both.wasm", &both), err(modsig::REVOKED));
    p.remove("/lib/keys/revoked").unwrap();
    assert_eq!(load(&mut p, "/bin/good.wasm", &good), Ok(()));

    // No list at all trusts nobody.
    p.remove("/lib/keys/modules").unwrap();
    assert_eq!(
        load(&mut p, "/bin/good.wasm", &good),
        err(modsig::UNTRUSTED)
    );
    assert_eq!(warnings(&mut p), "");

    // Only what loaded is recorded.
    let text = read_file(&mut p, &format!("#p/{pid}/module"));
    let names: Vec<&str> = text.lines().filter_map(|l| l.rsplit(' ').next()).collect();
    assert_eq!(
        names,
        [
            "require",
            "/bin/good.wasm",
            "/bin/both.wasm",
            "/bin/chain.wasm",
            "/bin/good.wasm",
            "/bin/both.wasm",
            "/bin/good.wasm"
        ]
    );
}

#[test]
fn the_level_only_tightens_and_children_inherit_it() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    let pid = p.pid;
    assert_eq!(set_level(&mut p, pid, "none"), Ok(()));
    assert_eq!(
        set_level(&mut p, pid, "sometimes"),
        Err(String::from(purg_abi::error::EBADARG))
    );
    let before = p.spawn(0, &[]).unwrap();
    assert_eq!(set_level(&mut p, pid, "warn"), Ok(()));
    assert_eq!(set_level(&mut p, pid, "warn"), Ok(()));
    assert_eq!(
        set_level(&mut p, pid, "none"),
        Err(String::from(purg_abi::error::EPERM))
    );
    let mut warned = p.spawn(0, &[]).unwrap();
    assert_eq!(set_level(&mut p, pid, "require"), Ok(()));
    for lower in ["warn", "none"] {
        assert_eq!(
            set_level(&mut p, pid, lower),
            Err(String::from(purg_abi::error::EPERM))
        );
    }
    assert_eq!(p.proc.module_policy().level, Level::Require);

    // A prog keeps the level it was spawned with; later children get the
    // new one, cannot lower it, and tightening one does not reach back.
    assert_eq!(before.proc.module_policy().level, Level::None);
    assert_eq!(warned.proc.module_policy().level, Level::Warn);
    let mut child = p
        .spawn(purg_abi::pctl::FORKNS | purg_abi::pctl::NEWPGRP, &[])
        .unwrap();
    assert_eq!(child.proc.module_policy().level, Level::Require);
    let cpid = child.pid;
    assert_eq!(
        set_level(&mut child, cpid, "warn"),
        Err(String::from(purg_abi::error::EPERM))
    );
    write_file(&mut p, "/bin/plain.wasm", &module(b"p"));
    assert_eq!(
        load_module(&mut child, "/bin/plain.wasm").map_err(|e| String::from(e.as_str())),
        Err(String::from(modsig::NOT_SIGNED))
    );
    let wpid = warned.pid;
    assert_eq!(set_level(&mut warned, wpid, "require"), Ok(()));
    assert_eq!(before.proc.module_policy().level, Level::None);
    let grandchild = child.spawn(purg_abi::pctl::NEWNS, &[]).unwrap();
    assert_eq!(grandchild.proc.module_policy().level, Level::Require);
}

#[test]
fn the_trust_lists_are_pinned_when_the_policy_is_set() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    write_file(
        &mut p,
        "/fake/modules",
        list_line(&public(&OTHER), "mine").as_bytes(),
    );
    let pid = p.pid;
    set_level(&mut p, pid, "require").unwrap();
    let mine = signed(b"mine", &OTHER);
    let good = signed(b"good", &SIGNER);
    assert_eq!(
        load(&mut p, "/bin/mine.wasm", &mine),
        Err(String::from(modsig::UNTRUSTED))
    );

    // The confined prog binds its own list over the real one. It reads
    // its own list at that name, but the loader does not.
    p.bind("/fake", "/lib/keys", MREPL).unwrap();
    assert!(read_file(&mut p, "/lib/keys/modules").contains("mine"));
    assert_eq!(
        load(&mut p, "/bin/mine.wasm", &mine),
        Err(String::from(modsig::UNTRUSTED))
    );
    assert_eq!(load(&mut p, "/bin/good.wasm", &good), Ok(()));
    // Nor does a child with a namespace of its own making.
    let mut child = p.spawn(purg_abi::pctl::FORKNS, &[]).unwrap();
    child.bind("/fake", "/lib", MREPL).unwrap();
    assert_eq!(
        load_module(&mut child, "/bin/mine.wasm").map_err(|e| String::from(e.as_str())),
        Err(String::from(modsig::UNTRUSTED))
    );
    assert!(load_module(&mut child, "/bin/good.wasm").is_ok());

    // A change to the real file takes effect at the next load.
    p.unmount(None, "/lib/keys").unwrap();
    trust(&mut p, &[&OTHER]);
    assert_eq!(load(&mut p, "/bin/mine.wasm", &mine), Ok(()));
    assert_eq!(
        load(&mut p, "/bin/good.wasm", &good),
        Err(String::from(modsig::UNTRUSTED))
    );

    // A prog that sets the policy with a bind already in place pins what
    // it sees then: that is its own choice of anchors.
    let (mut q, _) = setup();
    write_file(
        &mut q,
        "/fake/modules",
        list_line(&public(&OTHER), "").as_bytes(),
    );
    q.bind("/fake", "/lib/keys", MREPL).unwrap();
    let qpid = q.pid;
    set_level(&mut q, qpid, "warn").unwrap();
    q.unmount(None, "/lib/keys").unwrap();
    set_level(&mut q, qpid, "require").unwrap();
    assert_eq!(load(&mut q, "/bin/mine.wasm", &mine), Ok(()));
}

#[test]
fn a_writer_pins_the_target_progs_namespace() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    let mut child = p.spawn(purg_abi::pctl::FORKNS, &[]).unwrap();
    // The parent, with another list bound in its own namespace, sets the
    // child's level. The child's anchors are the child's.
    write_file(
        &mut p,
        "/fake/modules",
        list_line(&public(&OTHER), "").as_bytes(),
    );
    p.bind("/fake", "/lib/keys", MREPL).unwrap();
    let cpid = child.pid;
    set_level(&mut p, cpid, "require").unwrap();
    assert_eq!(p.proc.module_policy().level, Level::None);
    write_file(&mut child, "/bin/good.wasm", &signed(b"g", &SIGNER));
    write_file(&mut child, "/bin/mine.wasm", &signed(b"m", &OTHER));
    assert!(load_module(&mut child, "/bin/good.wasm").is_ok());
    assert_eq!(
        load_module(&mut child, "/bin/mine.wasm").map_err(|e| String::from(e.as_str())),
        Err(String::from(modsig::UNTRUSTED))
    );
}

#[test]
fn spawn_refuses_and_leaves_no_prog() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    write_file(&mut p, "/bin/plain.wasm", &module(b"p"));
    let pid = p.pid;
    set_level(&mut p, pid, "require").unwrap();
    let engine: Arc<dyn Engine> = Arc::new(NoEngine);
    let procs = p.kernel.procs.clone();
    let before = procs.pids();
    let e = purg_vm::spawn(&p, &engine, "/bin/plain.wasm", vec![], 0, &[]).unwrap_err();
    assert_eq!(e.as_str(), "/bin/plain.wasm: module not signed");
    assert_eq!(procs.pids(), before);
    // The refused spawn posts no exit message to the parent.
    assert!(!p.proc.wait_ready());
}

#[test]
fn the_system_default_covers_the_first_prog() {
    let (mut p, _) = setup();
    trust(&mut p, &[&SIGNER]);
    write_file(&mut p, "/bin/plain.wasm", &module(b"p"));
    write_file(&mut p, "/bin/good.wasm", &signed(b"g", &SIGNER));
    let procs = p.kernel.procs.clone();
    procs.set_module_policy(Level::Require, &p.pgrp);
    // The prog that was already there is not touched.
    assert_eq!(p.proc.module_policy().level, Level::None);
    let first = Prog::new(p.kernel.clone(), p.pgrp.clone());
    assert_eq!(first.proc.module_policy().level, Level::Require);
    let engine: Arc<dyn Engine> = Arc::new(NoEngine);
    let (ctx, res) =
        purg_vm::run_command(Ctx::new(first, vec![], engine.clone()), "/bin/plain.wasm");
    assert_eq!(
        res.unwrap_err().as_str(),
        "/bin/plain.wasm: module not signed"
    );
    // A signed first prog gets as far as the engine.
    let (_, res) = purg_vm::run_command(ctx, "/bin/good.wasm");
    assert_eq!(res.unwrap_err().as_str(), "/bin/good.wasm: not run");
    procs.set_module_policy(Level::None, &p.pgrp);
    let next = Prog::new(p.kernel.clone(), p.pgrp.clone());
    assert_eq!(next.proc.module_policy().level, Level::None);
}
