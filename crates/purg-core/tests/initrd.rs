//! Seeding a RamTree from a ustar initrd. `data/initrd.tar` is the output of
//! the `cargo xtask initrd` writer for the fixture in `xtask/src/tar.rs`, whose
//! `fixture_is_current` test keeps it in step.

use std::sync::Arc;

use purg_abi::{error, mode};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_styx::DMDIR;

static FIXTURE: &[u8] = include_bytes!("data/initrd.tar");

struct NullConsole;

impl Console for NullConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock(NullConsole);

thread_local! {
    static PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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
        &self.0
    }
    fn user(&self) -> String {
        "inferno".into()
    }
    fn sysname(&self) -> String {
        "purgatorio".into()
    }
    fn current_pid(&self) -> u32 {
        PID.get()
    }
    fn set_current_pid(&self, pid: u32) {
        PID.set(pid);
    }
    fn spawn_kproc(&self, _name: &str, _f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        Err(error::ENOPROC.into())
    }
}

fn ram() -> RamTree {
    RamTree::new('R', "ram", "inferno", Arc::new(Mock(NullConsole)))
}

fn prog(ram: RamTree) -> Prog {
    let platform: Arc<dyn Platform> = Arc::new(Mock(NullConsole));
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let p = Prog::new(kernel, pgrp);
    PID.set(p.pid);
    p
}

fn read_all(p: &mut Prog, path: &str) -> Vec<u8> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 300];
    loop {
        let n = p.read(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(fd).unwrap();
    out
}

fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut n: Vec<String> = p.dirents(fd).unwrap().into_iter().map(|d| d.name).collect();
    p.close(fd).unwrap();
    n.sort();
    n
}

#[test]
fn seeds_tree_from_xtask_archive() {
    let t = ram();
    assert_eq!(t.add_tar(FIXTURE).unwrap(), 10);
    let mut p = prog(t);
    let long = "d".repeat(60);
    assert_eq!(names(&mut p, "#R"), ["bin", "lib"]);
    assert_eq!(names(&mut p, "#R/bin"), ["lib", "sh.wasm"]);
    assert_eq!(names(&mut p, "#R/lib"), [long.as_str(), "empty", "motd"]);
    assert_eq!(read_all(&mut p, "#R/bin/sh.wasm"), b"\0asm\x01\0\0\0");
    assert_eq!(read_all(&mut p, "#R/bin/lib/add.wasm"), [7u8; 1000]);
    assert_eq!(
        read_all(&mut p, &format!("#R/lib/{long}/{long}/f.txt")),
        b"deep\n"
    );
    assert!(read_all(&mut p, "#R/lib/motd").is_empty());
    let d = p.stat("#R/bin/sh.wasm").unwrap();
    assert_eq!((d.mode, d.length), (0o755, 8));
    assert_eq!(p.stat("#R/lib/motd").unwrap().mode, 0o644);
    assert_eq!(p.stat("#R/lib/empty").unwrap().mode, DMDIR | 0o755);
    // Seeded files stay writable: the first write copies the borrowed data.
    let fd = p.open("#R/lib/motd", mode::OWRITE).unwrap();
    p.write(fd, b"hi").unwrap();
    p.close(fd).unwrap();
    assert_eq!(read_all(&mut p, "#R/lib/motd"), b"hi");
}

#[test]
fn data_is_borrowed() {
    let e = purg_core::initrd::entries(FIXTURE).unwrap();
    let range = FIXTURE.as_ptr_range();
    for e in e.iter().filter(|e| !e.data.is_empty()) {
        assert!(range.contains(&e.data.as_ptr()));
    }
}

#[test]
fn merges_with_existing_tree() {
    let t = ram();
    t.add_file("/bin/sh.wasm", &b"old"[..], 0o600);
    t.add_dir("/tmp");
    t.add_tar(FIXTURE).unwrap();
    let mut p = prog(t);
    assert_eq!(names(&mut p, "#R/bin"), ["lib", "sh.wasm"]);
    assert_eq!(names(&mut p, "#R"), ["bin", "lib", "tmp"]);
    assert_eq!(p.stat("#R/bin/sh.wasm").unwrap().mode, 0o755);
    assert_eq!(read_all(&mut p, "#R/bin/sh.wasm"), b"\0asm\x01\0\0\0");
}

#[test]
fn clash_and_malformed() {
    let t = ram();
    t.add_file("/lib", &b"a file"[..], 0o644);
    assert_eq!(t.add_tar(FIXTURE).unwrap_err().as_str(), error::EEXIST);

    // A bad header late in the archive adds nothing, not even the entries
    // before it. The last entry, the empty lib/motd, has its header just
    // before the two end-of-archive blocks.
    let mut bad = FIXTURE.to_vec();
    let last = bad.len() - 3 * 512;
    assert_eq!(&bad[last..last + 8], b"lib/motd");
    bad[last] ^= 1;
    let bad: &'static [u8] = Box::leak(bad.into_boxed_slice());
    let t = ram();
    assert!(t.add_tar(bad).unwrap_err().as_str().contains("checksum"));
    let mut p = prog(t);
    assert!(names(&mut p, "#R").is_empty());
}
