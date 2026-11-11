//! `#K` through the namespace: what a prog sees of a mounted purgfs.

use std::sync::Arc;

use purg_abi::mode;
use purg_abi::mount::{MCREATE, MREPL};
use purg_core::devroot::StaticTree;
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_fs::dev::{DevFs, EEXCL};
use purg_fs::layout::DMEXCL;
use purg_fs::mem::MemDisk;
use purg_fs::{Format, Fs};
use purg_platform::Console;
use purg_styx::{DMDIR, Dir, QTDIR};

struct MockConsole;

impl Console for MockConsole {
    fn read(&self, _buf: &mut [u8]) -> purg_platform::Result<usize> {
        Ok(0)
    }
    fn write(&self, buf: &[u8]) -> purg_platform::Result<usize> {
        Ok(buf.len())
    }
}

struct Mock(MockConsole);

impl Platform for Mock {
    fn now_ns(&self) -> u64 {
        1_700_000_000 * 1_000_000_000
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
        0
    }
    fn set_current_pid(&self, _pid: u32) {}
    fn spawn_kproc(&self, _name: &str, _f: Box<dyn FnOnce() + Send>) -> purg_platform::Result<()> {
        Err(purg_abi::error::ENOPROC.into())
    }
}

fn setup() -> (Prog, Arc<DevFs<MemDisk>>) {
    let platform: Arc<dyn Platform> = Arc::new(Mock(MockConsole));
    let fs = Fs::format(MemDisk::new(2 << 20, 512), &Format::default()).unwrap();
    let dev = Arc::new(DevFs::new(fs, platform.clone()));
    let procs = Procs::new(platform.clone());
    let mut root = StaticTree::root("inferno");
    root.add_dir("n/disk");
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(root)];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    devs.push(dev.clone());
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    pgrp.bind("#K", "/n/disk", MREPL | MCREATE).unwrap();
    (Prog::new(kernel, pgrp), dev)
}

fn put(p: &mut Prog, path: &str, data: &[u8]) {
    let fd = p.create(path, mode::OWRITE, 0o664).unwrap();
    assert_eq!(p.write(fd, data).unwrap(), data.len());
    p.close(fd).unwrap();
}

fn get(p: &mut Prog, path: &str) -> Vec<u8> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 1000];
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

fn ls(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut names = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = p.dirread(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        let mut off = 0;
        while off < n {
            let (d, used) = Dir::unpack(&buf[off..n]).unwrap();
            names.push(d.name.to_string());
            off += used;
        }
    }
    p.close(fd).unwrap();
    names.sort();
    names
}

#[test]
fn files_through_the_namespace() {
    let (mut p, dev) = setup();
    let fd = p.create("/n/disk/d", mode::OREAD, DMDIR | 0o775).unwrap();
    p.close(fd).unwrap();
    put(&mut p, "/n/disk/d/a", b"one\n");
    put(&mut p, "/n/disk/b", &vec![7u8; 100_000]);
    assert_eq!(get(&mut p, "/n/disk/d/a"), b"one\n");
    assert_eq!(get(&mut p, "/n/disk/b"), vec![7u8; 100_000]);
    assert_eq!(ls(&mut p, "/n/disk"), ["b", "d"]);
    let d = p.stat("/n/disk/d").unwrap();
    assert!(d.qid.qtype & QTDIR != 0 && d.mode & DMDIR != 0 && d.length == 0);
    assert_eq!((d.uid.as_str(), d.gid.as_str()), ("inferno", "inferno"));
    let b = p.stat("/n/disk/b").unwrap();
    assert_eq!((b.length, b.mode, b.dtype), (100_000, 0o664, 'K' as u16));
    assert_eq!(b.mtime, 1_700_000_000);
    // A write moves the qid version.
    let fd = p.open("/n/disk/b", mode::OWRITE).unwrap();
    p.write(fd, b"x").unwrap();
    p.close(fd).unwrap();
    assert!(p.stat("/n/disk/b").unwrap().qid.vers > b.qid.vers);
    // Rename, truncate and chmod by wstat.
    let mut w = Dir::null();
    w.name = "c".into();
    p.wstat("/n/disk/b", &w).unwrap();
    let mut w = Dir::null();
    w.length = 10;
    w.mode = 0o600;
    p.wstat("/n/disk/c", &w).unwrap();
    let c = p.stat("/n/disk/c").unwrap();
    assert_eq!((c.length, c.mode), (10, 0o600));
    assert_eq!(
        p.stat("/n/disk/b").unwrap_err().as_str(),
        "file does not exist"
    );
    // Errors, as Inferno words them.
    let err = |r: purg_core::Result<()>| r.unwrap_err().as_str().to_string();
    assert_eq!(err(p.remove("/n/disk/d")), "directory not empty");
    let mut w = Dir::null();
    w.uid = "glenda".into();
    assert_eq!(err(p.wstat("/n/disk/c", &w)), "permission denied");
    let mut w = Dir::null();
    w.name = "d".into();
    assert_eq!(err(p.wstat("/n/disk/c", &w)), "file already exists");
    assert_eq!(
        p.open("/n/disk/d", mode::OWRITE).unwrap_err().as_str(),
        "file is a directory"
    );
    // `>` on an existing file truncates it.
    put(&mut p, "/n/disk/c", b"new");
    assert_eq!(get(&mut p, "/n/disk/c"), b"new");
    p.remove("/n/disk/d/a").unwrap();
    p.remove("/n/disk/d").unwrap();
    p.remove("/n/disk/c").unwrap();
    assert!(ls(&mut p, "/n/disk").is_empty());
    assert!(dev.check().ok());
}

#[test]
fn open_modes() {
    let (mut p, dev) = setup();
    put(&mut p, "/n/disk/f", b"0123456789");
    // OTRUNC.
    let fd = p.open("/n/disk/f", mode::OWRITE | mode::OTRUNC).unwrap();
    p.close(fd).unwrap();
    assert_eq!(p.stat("/n/disk/f").unwrap().length, 0);
    // ORCLOSE removes at the last close.
    let fd = p
        .create("/n/disk/tmp", mode::OWRITE | mode::ORCLOSE, 0o664)
        .unwrap();
    p.write(fd, b"gone soon").unwrap();
    assert_eq!(ls(&mut p, "/n/disk"), ["f", "tmp"]);
    p.close(fd).unwrap();
    assert_eq!(ls(&mut p, "/n/disk"), ["f"]);
    // DMEXCL: one open at a time.
    let mut w = Dir::null();
    w.mode = 0o664 | DMEXCL;
    p.wstat("/n/disk/f", &w).unwrap();
    let fd = p.open("/n/disk/f", mode::OREAD).unwrap();
    assert_eq!(
        p.open("/n/disk/f", mode::OREAD).unwrap_err().as_str(),
        EEXCL
    );
    p.close(fd).unwrap();
    let fd = p.open("/n/disk/f", mode::OREAD).unwrap();
    p.close(fd).unwrap();
    // An fd on a removed file fails.
    put(&mut p, "/n/disk/g", b"g");
    let fd = p.open("/n/disk/g", mode::OREAD).unwrap();
    p.remove("/n/disk/g").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(
        p.read(fd, &mut buf).unwrap_err().as_str(),
        "file does not exist"
    );
    p.close(fd).unwrap();
    assert!(dev.check().ok());
}
