//! The mount driver never walks from a fid it has opened. 9P2000 forbids
//! it, and servers such as Plan 9's kfs refuse it. Inferno's devmnt opens
//! a clone of a mount's root and leaves the root itself unopened; this
//! checks ours against a server that refuses a walk, or an open, of an
//! open fid.
#![cfg(feature = "std")]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread;

use common::setup;
use purg_abi::mount::{MAFTER, MNOAUTH, MREPL};
use purg_abi::{error, mode};
use purg_core::Prog;
use purg_core::rw::{read_msg, write_full};
use purg_styx::{DMDIR, Dir, MAX_MSIZE, Msg, QTDIR, QTFILE, Qid, StyxFs, dispatch};

/// The tree: `/a`, `/d` and `/d/e`. Each entry is (path, name, parent).
const TREE: [(u64, &str, u64); 4] = [(0, "/", 0), (1, "a", 0), (2, "d", 0), (3, "e", 2)];

fn is_dir(path: u64) -> bool {
    path == 0 || path == 2
}

fn qid(path: u64) -> Qid {
    Qid {
        path,
        vers: 0,
        qtype: if is_dir(path) { QTDIR } else { QTFILE },
    }
}

fn dir(path: u64) -> Dir {
    let (_, name, _) = TREE[path as usize];
    Dir {
        qid: qid(path),
        mode: if is_dir(path) { DMDIR | 0o555 } else { 0o444 },
        length: if is_dir(path) { 0 } else { 1 },
        name: String::from(name),
        uid: String::from("strict"),
        gid: String::from("strict"),
        muid: String::from("strict"),
        ..Dir::default()
    }
}

/// A server that keeps 9P2000's rule on open fids, with the errors
/// Inferno's export gives.
#[derive(Default)]
struct Strict {
    /// fid -> (path, open).
    fids: BTreeMap<u32, (u64, bool)>,
    /// Requests refused for using an open fid.
    refused: Arc<Mutex<Vec<String>>>,
}

impl Strict {
    fn get(&self, fid: u32) -> Result<(u64, bool), String> {
        self.fids
            .get(&fid)
            .copied()
            .ok_or_else(|| "unknown fid".into())
    }

    fn refuse(&self, what: &str) -> Result<(), String> {
        self.refused.lock().unwrap().push(String::from(what));
        Err(String::from(what))
    }
}

impl StyxFs for Strict {
    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        self.fids.insert(fid, (0, false));
        Ok(qid(0))
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        let (mut at, open) = self.get(fid)?;
        if open {
            self.refuse("walk of open fid")?;
        }
        let mut qids = Vec::new();
        for name in names {
            let next = match *name {
                ".." => TREE[at as usize].2,
                n => match TREE.iter().find(|e| e.0 != 0 && e.2 == at && e.1 == n) {
                    Some(e) if is_dir(at) => e.0,
                    _ if qids.is_empty() => return Err(error::ENONEXIST.into()),
                    _ => return Ok(qids),
                },
            };
            at = next;
            qids.push(qid(at));
        }
        self.fids.insert(newfid, (at, false));
        Ok(qids)
    }

    fn open(&mut self, fid: u32, _mode: u8) -> Result<(Qid, u32), String> {
        let (path, open) = self.get(fid)?;
        if open {
            self.refuse("open/create -- unknown mode")?;
        }
        self.fids.insert(fid, (path, true));
        Ok((qid(path), 0))
    }

    fn create(&mut self, _: u32, _: &str, _: u32, _: u8) -> Result<(Qid, u32), String> {
        Err(error::EPERM.into())
    }

    fn read(&mut self, fid: u32, offset: u64, _count: u32) -> Result<Vec<u8>, String> {
        let (path, open) = self.get(fid)?;
        if !open {
            return Err(String::from("read/write -- not open in suitable mode"));
        }
        let mut out = Vec::new();
        if offset != 0 {
            return Ok(out);
        }
        if is_dir(path) {
            for e in TREE.iter().filter(|e| e.0 != 0 && e.2 == path) {
                dir(e.0).pack_into(&mut out).unwrap();
            }
        } else {
            out.extend_from_slice(TREE[path as usize].1.as_bytes());
        }
        Ok(out)
    }

    fn write(&mut self, _: u32, _: u64, _: &[u8]) -> Result<u32, String> {
        Err(error::EPERM.into())
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids.remove(&fid).ok_or("unknown fid")?;
        Ok(())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(error::EPERM.into())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let (path, _) = self.get(fid)?;
        dir(path).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _: u32, _: &[u8]) -> Result<(), String> {
        Err(error::EPERM.into())
    }
}

/// Mounts a `Strict` server on `#R/mnt`; returns what it refused.
fn mount_strict(p: &mut Prog) -> Arc<Mutex<Vec<String>>> {
    let refused = Arc::new(Mutex::new(Vec::new()));
    let mut fs = Strict {
        refused: refused.clone(),
        ..Strict::default()
    };
    let [srv, cli] = p.pipe().unwrap();
    let fd = p.fgrp.get(srv).unwrap();
    p.close(srv).unwrap();
    thread::spawn(move || {
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
    refused
}

fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let names = p.dirents(fd).unwrap().into_iter().map(|d| d.name).collect();
    p.close(fd).unwrap();
    names
}

fn read_all(p: &mut Prog, path: &str) -> String {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut buf = [0u8; 64];
    let n = p.read(fd, &mut buf).unwrap();
    p.close(fd).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

/// `ls` of the mount point, then walks and opens below it, again and
/// again, with directories held open while others are walked.
#[test]
fn no_walk_or_open_of_an_open_fid() {
    let (mut p, _mock) = setup();
    let refused = mount_strict(&mut p);
    for _ in 0..3 {
        assert_eq!(names(&mut p, "#R/mnt"), ["a", "d"]);
        assert_eq!(read_all(&mut p, "#R/mnt/a"), "a");
        assert_eq!(names(&mut p, "#R/mnt/d"), ["e"]);
        assert_eq!(read_all(&mut p, "#R/mnt/d/e"), "e");
        assert_eq!(p.stat("#R/mnt/d/e").unwrap().name, "e");
    }
    // The mount point's directory open while names below it are walked.
    let held = p.open("#R/mnt", mode::OREAD).unwrap();
    let sub = p.open("#R/mnt/d", mode::OREAD).unwrap();
    assert_eq!(read_all(&mut p, "#R/mnt/d/e"), "e");
    assert_eq!(names(&mut p, "#R/mnt"), ["a", "d"]);
    p.close(sub).unwrap();
    p.close(held).unwrap();
    // From the mount as the current directory, and through a union.
    p.chdir("#R/mnt/d").unwrap();
    assert_eq!(names(&mut p, "."), ["e"]);
    assert_eq!(read_all(&mut p, "e"), "e");
    p.chdir("#R/mnt").unwrap();
    assert_eq!(names(&mut p, "."), ["a", "d"]);
    assert_eq!(read_all(&mut p, "d/e"), "e");
    p.chdir("/").unwrap();
    p.bind("#R/box", "#R/mnt", MAFTER).unwrap();
    assert_eq!(names(&mut p, "#R/mnt"), ["a", "d"]);
    assert_eq!(read_all(&mut p, "#R/mnt/d/e"), "e");
    assert_eq!(*refused.lock().unwrap(), Vec::<String>::new());
}
