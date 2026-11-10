//! The mount table holds the Chan it mounts on (issue #47).
//!
//! The file server here behaves like mntgen(4): every name walked to is an
//! empty directory that exists while some fid refers to it. A name nobody
//! refers to is forgotten, so the next walk to it may get another qid, and
//! its qid may be given to another name. Those are legal for a 9P server.
//! A mount on such a name keeps working only if the mount holds a fid on
//! it.
#![cfg(feature = "std")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use purg_abi::mount::{MAFTER, MBEFORE, MNOAUTH, MREPL};
use purg_abi::{error, mode, pctl};
use purg_core::devram::RamTree;
use purg_core::devroot::StaticTree;
use purg_core::rw::{read_msg, write_full};
use purg_core::{Dev, DevTab, Kernel, Pgrp, Platform, Procs, Prog};
use purg_platform::Console;
use purg_styx::{DMDIR, Dir, MAX_MSIZE, Msg, QTDIR, Qid, StyxFs, dispatch};

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

/// How the server numbers a name it has no entry for.
#[derive(Clone, Copy)]
enum Paths {
    /// A path never used before.
    Fresh,
    /// The lowest path no live name has.
    Lowest,
}

/// A mntgen-like server. Names live while a fid refers to them.
struct Gen {
    paths: Paths,
    next: u64,
    names: BTreeMap<String, (u64, usize)>,
    fids: BTreeMap<u32, Option<String>>,
}

const ROOT: u64 = 0;

impl Gen {
    fn new(paths: Paths) -> Gen {
        Gen {
            paths,
            next: 1,
            names: BTreeMap::new(),
            fids: BTreeMap::new(),
        }
    }

    fn qid(path: u64) -> Qid {
        Qid {
            path,
            vers: 0,
            qtype: QTDIR,
        }
    }

    fn path(&mut self, name: &str) -> u64 {
        if let Some((p, _)) = self.names.get(name) {
            return *p;
        }
        let p = match self.paths {
            Paths::Fresh => {
                self.next += 1;
                self.next - 1
            }
            Paths::Lowest => (1..)
                .find(|p| self.names.values().all(|(q, _)| q != p))
                .unwrap(),
        };
        self.names.insert(String::from(name), (p, 0));
        p
    }

    fn hold(&mut self, fid: u32, at: Option<String>) {
        if let Some(n) = &at {
            self.path(n);
            self.names.get_mut(n).unwrap().1 += 1;
        }
        if let Some(old) = self.fids.insert(fid, at) {
            self.release(old);
        }
    }

    fn release(&mut self, at: Option<String>) {
        let Some(n) = at else { return };
        let e = self.names.get_mut(&n).unwrap();
        e.1 -= 1;
        if e.1 == 0 {
            self.names.remove(&n);
        }
    }

    fn dir(name: &str, path: u64) -> Dir {
        Dir {
            qid: Gen::qid(path),
            mode: DMDIR | 0o555,
            name: String::from(name),
            uid: String::from("gen"),
            gid: String::from("gen"),
            muid: String::from("gen"),
            ..Dir::default()
        }
    }
}

impl StyxFs for Gen {
    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        self.hold(fid, None);
        Ok(Gen::qid(ROOT))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        let mut at = self.fids.get(&fid).ok_or("unknown fid")?.clone();
        let mut qids = Vec::new();
        for name in names {
            at = match (&at, *name) {
                (_, "..") => None,
                (None, n) => Some(String::from(n)),
                (Some(_), _) if qids.is_empty() => return Err(error::ENONEXIST.into()),
                (Some(_), _) => return Ok(qids),
            };
            qids.push(Gen::qid(match &at {
                None => ROOT,
                Some(n) => self.path(n),
            }));
        }
        // A name walked through but not kept by any fid is forgotten.
        let walked: Vec<String> = self
            .names
            .iter()
            .filter(|(_, (_, refs))| *refs == 0)
            .map(|(n, _)| n.clone())
            .collect();
        self.hold(newfid, at);
        for n in walked {
            if self.names.get(&n).is_some_and(|e| e.1 == 0) {
                self.names.remove(&n);
            }
        }
        Ok(qids)
    }

    fn open(&mut self, fid: u32, _mode: u8) -> Result<(Qid, u32), String> {
        let at = self.fids.get(&fid).ok_or("unknown fid")?;
        let path = at.as_ref().map_or(ROOT, |n| self.names[n].0);
        Ok((Gen::qid(path), 8192))
    }

    fn create(&mut self, _: u32, _: &str, _: u32, _: u8) -> Result<(Qid, u32), String> {
        Err(error::EPERM.into())
    }

    fn read(&mut self, fid: u32, offset: u64, _count: u32) -> Result<Vec<u8>, String> {
        let at = self.fids.get(&fid).ok_or("unknown fid")?;
        let mut out = Vec::new();
        if at.is_none() && offset == 0 {
            for (n, (p, _)) in &self.names {
                Gen::dir(n, *p).pack_into(&mut out).unwrap();
            }
        }
        Ok(out)
    }

    fn write(&mut self, _: u32, _: u64, _: &[u8]) -> Result<u32, String> {
        Err(error::EPERM.into())
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        let at = self.fids.remove(&fid).ok_or("unknown fid")?;
        self.release(at);
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(error::EPERM.into())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let at = self.fids.get(&fid).ok_or("unknown fid")?;
        let d = match at {
            None => Gen::dir("/", ROOT),
            Some(n) => Gen::dir(n, self.names[n].0),
        };
        d.pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _: u32, _: &[u8]) -> Result<(), String> {
        Err(error::EPERM.into())
    }
}

/// A prog with `#A` and `#B` static trees and a `Gen` server mounted on
/// `#R/mnt`.
fn setup(paths: Paths) -> Prog {
    let mock = Arc::new(Mock {
        console: MockConsole,
    });
    let platform: Arc<dyn Platform> = mock.clone();
    let procs = Procs::new(platform.clone());
    let mut devs: Vec<Arc<dyn Dev>> = vec![Arc::new(StaticTree::root("inferno"))];
    let portable = purg_core::portable_devices(&platform, &procs);
    devs.extend(portable.devs);
    let mut a = StaticTree::new('A', "a", "a");
    a.add_file("/x", &b"a's x"[..]);
    a.add_file("/only-a", &b"in a"[..]);
    a.add_file("/sub/deep", &b"deep"[..]);
    devs.push(Arc::new(a));
    let mut b = StaticTree::new('B', "b", "b");
    b.add_file("/x", &b"b's x"[..]);
    b.add_file("/only-b", &b"in b"[..]);
    devs.push(Arc::new(b));
    let ram = RamTree::new('R', "ram", "inferno", platform.clone());
    ram.add_dir("/mnt");
    devs.push(Arc::new(ram));
    let devtab = DevTab::new(devs);
    let kernel = Kernel::new(platform, devtab.clone(), procs, portable.srv, portable.mnt);
    let pgrp = Pgrp::new(devtab.clone(), devtab.attach('/', "").unwrap());
    let mut p = Prog::new(kernel, pgrp);
    mock.set_current_pid(p.pid);

    let [srv, cli] = p.pipe().unwrap();
    let fd = p.fgrp.get(srv).unwrap();
    p.close(srv).unwrap();
    thread::spawn(move || {
        let mut fs = Gen::new(paths);
        let mut msize = MAX_MSIZE;
        while let Ok(buf) = read_msg(&fd, msize) {
            let msg = Msg::decode(&buf, msize).unwrap();
            let reply = dispatch(&mut fs, &msg, &mut msize);
            if write_full(&fd, &reply).is_err() {
                break;
            }
        }
    });
    p.mount(cli, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(cli).unwrap();
    p
}

fn read_all(p: &mut Prog, path: &str) -> Result<String, String> {
    let fd = p.open(path, mode::OREAD).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = p.read(fd, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(fd).unwrap();
    Ok(String::from_utf8(out).unwrap())
}

fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let names = p.dirents(fd).unwrap().into_iter().map(|d| d.name).collect();
    p.close(fd).unwrap();
    names
}

/// The mntgen case from #15: a name that only the mount refers to.
#[test]
fn a_name_only_mounted_on_stays_mounted() {
    let mut p = setup(Paths::Fresh);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/x/x").unwrap(), "a's x");
    assert_eq!(read_all(&mut p, "#R/mnt/x/sub/deep").unwrap(), "deep");
    // The server lists it: the mount table's fid keeps it alive.
    assert_eq!(names(&mut p, "#R/mnt"), ["x"]);
    p.bind("#B", "#R/mnt/y", MREPL).unwrap();
    assert_eq!(names(&mut p, "#R/mnt"), ["x", "y"]);
    assert_eq!(read_all(&mut p, "#R/mnt/y/x").unwrap(), "b's x");
}

/// A server may give a forgotten name's qid to another name. A mount must
/// not appear on that other name.
#[test]
fn a_reused_qid_does_not_alias_a_mount() {
    let mut p = setup(Paths::Lowest);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    assert!(names(&mut p, "#R/mnt/y").is_empty());
    assert_eq!(read_all(&mut p, "#R/mnt/x/only-a").unwrap(), "in a");
    assert_eq!(
        read_all(&mut p, "#R/mnt/y/only-a").unwrap_err(),
        error::ENONEXIST
    );
}

#[test]
fn unmount_by_the_mounted_on_name() {
    let mut p = setup(Paths::Fresh);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    p.unmount(None, "#R/mnt/x").unwrap();
    // The mount's reference is gone, so the server forgets the name.
    assert!(names(&mut p, "#R/mnt").is_empty());
    assert_eq!(
        p.unmount(None, "#R/mnt/x").unwrap_err().as_str(),
        error::EUNMOUNT
    );

    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    p.bind("#B", "#R/mnt/x", MAFTER).unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/x/only-b").unwrap(), "in b");
    p.unmount(Some("#B"), "#R/mnt/x").unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/x/x").unwrap(), "a's x");
    assert_eq!(
        read_all(&mut p, "#R/mnt/x/only-b").unwrap_err(),
        error::ENONEXIST
    );
    p.unmount(Some("#A"), "#R/mnt/x").unwrap();
    assert!(names(&mut p, "#R/mnt").is_empty());
}

#[test]
fn union_order_on_a_mounted_on_name() {
    let mut p = setup(Paths::Fresh);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    p.bind("#B", "#R/mnt/x", MBEFORE).unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/x/x").unwrap(), "b's x");
    assert_eq!(read_all(&mut p, "#R/mnt/x/only-a").unwrap(), "in a");
    assert_eq!(
        names(&mut p, "#R/mnt/x"),
        ["x", "only-b", "x", "only-a", "sub"]
    );

    // A union on a name not yet mounted on includes the original (empty)
    // directory, which `ns` does not list.
    p.bind("#B", "#R/mnt/y", MAFTER).unwrap();
    p.bind("#A", "#R/mnt/y", MAFTER).unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/y/x").unwrap(), "b's x");
    assert_eq!(
        names(&mut p, "#R/mnt/y"),
        ["x", "only-b", "x", "only-a", "sub"]
    );
    let ns = p.pgrp.namespace();
    // Members are listed in search order.
    let want = "bind -b '#B' '#R/mnt/x'\n\
                bind '#A' '#R/mnt/x'\n\
                bind -a '#B' '#R/mnt/y'\n\
                bind -a '#A' '#R/mnt/y'\n\
                cd /\n";
    assert!(ns.ends_with(want), "{ns}");
}

#[test]
fn dotdot_leaves_a_mount() {
    let mut p = setup(Paths::Fresh);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    p.chdir("#R/mnt/x/sub").unwrap();
    assert_eq!(read_all(&mut p, "deep").unwrap(), "deep");
    assert_eq!(read_all(&mut p, "../x").unwrap(), "a's x");
    assert_eq!(names(&mut p, ".."), ["x", "only-a", "sub"]);
    assert_eq!(names(&mut p, "../.."), ["x"]);
    let fd = p.open("../..", mode::OREAD).unwrap();
    assert_eq!(p.fd2path(fd).unwrap(), "#R/mnt");
    p.close(fd).unwrap();
}

#[test]
fn a_forked_namespace_shares_the_mounted_on_chan() {
    let mut p = setup(Paths::Fresh);
    p.bind("#A", "#R/mnt/x", MREPL).unwrap();
    let mut forked = p.spawn(pctl::FORKNS, &[]).unwrap();
    p.unmount(None, "#R/mnt/x").unwrap();
    assert!(names(&mut p, "#R/mnt/x").is_empty());
    assert_eq!(read_all(&mut forked, "#R/mnt/x/x").unwrap(), "a's x");
    // The forked namespace still holds the name.
    assert_eq!(names(&mut p, "#R/mnt"), ["x"]);
    forked.pctl(pctl::NEWNS, &[]).unwrap();
    assert!(names(&mut p, "#R/mnt").is_empty());
}

/// The current directory is a held Chan (issue #55): a name that only a
/// prog's `dot` refers to stays alive on the server.
#[test]
fn the_current_directory_keeps_its_name_alive() {
    let mut p = setup(Paths::Fresh);
    p.chdir("#R/mnt/x").unwrap();
    assert_eq!(names(&mut p, "#R/mnt"), ["x"]);
    p.chdir("/").unwrap();
    assert_eq!(names(&mut p, "#R/mnt"), Vec::<String>::new());
}
