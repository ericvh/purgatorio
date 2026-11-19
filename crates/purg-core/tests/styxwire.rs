//! What the mount client (`#M`) puts on the wire (issue #69).
#![cfg(feature = "std")]

mod common;

use std::sync::{Arc, Mutex};
use std::thread;

use common::setup;
use purg_abi::mount::{MNOAUTH, MREPL};
use purg_abi::{error, mode};
use purg_core::Prog;
use purg_core::rw::{read_msg, write_full};
use purg_styx::{DMDIR, Dir, MAX_MSIZE, Msg, NOTAG, QTDIR, Qid, StyxFs, dispatch};

/// A server with one directory holding one file, `f`.
struct One {
    fids: std::collections::BTreeMap<u32, bool>,
}

const FILE: Qid = Qid {
    path: 1,
    vers: 0,
    qtype: 0,
};
const ROOT: Qid = Qid {
    path: 0,
    vers: 0,
    qtype: QTDIR,
};

impl StyxFs for One {
    fn attach(&mut self, fid: u32, _afid: u32, _uname: &str, _aname: &str) -> Result<Qid, String> {
        self.fids.insert(fid, false);
        Ok(ROOT)
    }

    fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> Result<Vec<Qid>, String> {
        let at = *self.fids.get(&fid).ok_or("unknown fid")?;
        match names {
            [] => {
                self.fids.insert(newfid, at);
                Ok(Vec::new())
            }
            ["f"] if !at => {
                self.fids.insert(newfid, true);
                Ok(vec![FILE])
            }
            _ if at => Err(error::ENOTDIR.into()),
            _ => Err(error::ENONEXIST.into()),
        }
    }

    fn open(&mut self, fid: u32, _mode: u8) -> Result<(Qid, u32), String> {
        let file = *self.fids.get(&fid).ok_or("unknown fid")?;
        Ok((if file { FILE } else { ROOT }, 0))
    }

    fn create(&mut self, _: u32, _: &str, _: u32, _: u8) -> Result<(Qid, u32), String> {
        Err(error::EPERM.into())
    }

    fn read(&mut self, fid: u32, offset: u64, _: u32) -> Result<Vec<u8>, String> {
        let file = *self.fids.get(&fid).ok_or("unknown fid")?;
        if file || offset != 0 {
            return Ok(Vec::new());
        }
        dir(true).pack().map_err(|e| e.to_string())
    }

    fn write(&mut self, _: u32, _: u64, _: &[u8]) -> Result<u32, String> {
        Err(error::EPERM.into())
    }

    fn clunk(&mut self, fid: u32) -> Result<(), String> {
        self.fids.remove(&fid).map(drop).ok_or("unknown fid".into())
    }

    fn remove(&mut self, fid: u32) -> Result<(), String> {
        self.clunk(fid)?;
        Err(error::EPERM.into())
    }

    fn stat(&mut self, fid: u32) -> Result<Vec<u8>, String> {
        let file = *self.fids.get(&fid).ok_or("unknown fid")?;
        dir(file).pack().map_err(|e| e.to_string())
    }

    fn wstat(&mut self, _: u32, _: &[u8]) -> Result<(), String> {
        Err(error::EPERM.into())
    }
}

/// The server's Dir for the root or `f`, with a device type and number
/// the mount driver must replace.
fn dir(file: bool) -> Dir {
    Dir {
        dtype: 7,
        dev: 99,
        qid: if file { FILE } else { ROOT },
        mode: if file { 0o444 } else { DMDIR | 0o555 },
        name: String::from(if file { "f" } else { "/" }),
        ..Dir::default()
    }
}

/// One request the server saw, and whether it answered with `Rerror`.
struct Seen {
    tag: u16,
    text: String,
    failed: bool,
}

/// Mounts a [`One`] server on `#R/mnt` and returns what it is sent.
fn mount_logged(p: &mut Prog) -> Arc<Mutex<Vec<Seen>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let [srv, cli] = p.pipe().unwrap();
    let fd = p.fgrp.get(srv).unwrap();
    p.close(srv).unwrap();
    let seen = log.clone();
    thread::spawn(move || {
        let mut fs = One {
            fids: Default::default(),
        };
        let mut msize = MAX_MSIZE;
        while let Ok(buf) = read_msg(&fd, msize) {
            let msg = Msg::decode(&buf, msize).unwrap();
            let reply = dispatch(&mut fs, &msg, &mut msize);
            seen.lock().unwrap().push(Seen {
                tag: msg.tag,
                text: format!("{:?}", msg.fcall),
                failed: reply[4] == purg_styx::kind::RERROR,
            });
            if write_full(&fd, &reply).is_err() {
                break;
            }
        }
    });
    p.mount(cli, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(cli).unwrap();
    log
}

/// `Tversion` carries `NOTAG`, as version(5) requires.
#[test]
fn tversion_is_sent_with_notag() {
    let (mut p, _) = setup();
    let log = mount_logged(&mut p);
    let log = log.lock().unwrap();
    assert!(log[0].text.starts_with("Tversion"), "{}", log[0].text);
    assert_eq!(log[0].tag, NOTAG);
    assert!(log[1..].iter().all(|s| s.tag != NOTAG && !s.failed));
}

/// A walk that fails does not create its new fid, so there is nothing to
/// clunk: the only requests the server refuses are the walks.
#[test]
fn a_failed_walk_sends_no_clunk() {
    let (mut p, _) = setup();
    let log = mount_logged(&mut p);
    let err = p.open("#R/mnt/missing", mode::OREAD).unwrap_err();
    assert_eq!(err.as_str(), error::ENONEXIST);
    // A round trip, so everything the failures sent has been logged.
    p.stat("#R/mnt/f").unwrap();
    let log = log.lock().unwrap();
    let refused: Vec<&str> = log
        .iter()
        .filter(|s| s.failed)
        .map(|s| s.text.as_str())
        .collect();
    assert!(!refused.is_empty());
    for text in refused {
        assert!(text.starts_with("Twalk"), "refused: {text}");
    }
}

/// A server that goes away with a request outstanding fails that request
/// and every later one; nothing waits for a reply that cannot come.
#[test]
fn a_server_that_hangs_up_fails_requests() {
    let (mut p, _) = setup();
    let [srv, cli] = p.pipe().unwrap();
    let fd = p.fgrp.get(srv).unwrap();
    p.close(srv).unwrap();
    thread::spawn(move || {
        let mut fs = One {
            fids: Default::default(),
        };
        let mut msize = MAX_MSIZE;
        while let Ok(buf) = read_msg(&fd, msize) {
            let msg = Msg::decode(&buf, msize).unwrap();
            if matches!(msg.fcall, purg_styx::Fcall::Tstat { .. }) {
                break;
            }
            let reply = dispatch(&mut fs, &msg, &mut msize);
            if write_full(&fd, &reply).is_err() {
                break;
            }
        }
    });
    p.mount(cli, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(cli).unwrap();
    let err = p.stat("#R/mnt/f").unwrap_err();
    assert_eq!(err.as_str(), error::EHUNGUP);
    let err = p.stat("#R/mnt/f").unwrap_err();
    assert_eq!(err.as_str(), error::EHUNGUP);
}

/// Every Dir from a mounted server, by stat or directory read, carries
/// `#M`'s type and the mount's number, whatever the server sent (#165).
#[test]
fn mounted_dirs_carry_the_mount_device() {
    let (mut p, _) = setup();
    let _log = mount_logged(&mut p);
    let root = p.stat("#R/mnt").unwrap();
    let f = p.stat("#R/mnt/f").unwrap();
    let fd = p.open("#R/mnt", mode::OREAD).unwrap();
    let mut buf = vec![0u8; 8192];
    let n = p.dirread(fd, &mut buf).unwrap();
    p.close(fd).unwrap();
    let (entry, _) = Dir::unpack(&buf[..n]).unwrap();
    assert_eq!(entry.name, "f");
    for d in [&root, &f, &entry] {
        assert_eq!(d.dtype, u16::from(b'M'), "{d:?}");
        assert_ne!(d.dev, 99, "{d:?}");
        assert_eq!(d.dev, root.dev);
    }
}

/// What a mount without authentication sends before anything is asked of
/// it: the version, the attach and one stat of the root, which is the
/// check for a server that cannot answer a stat at this msize (issue
/// #313). The attach is the mount's own: there is no second one.
#[test]
fn a_mount_stats_the_root_once() {
    let (mut p, _) = setup();
    let log = mount_logged(&mut p);
    let log = log.lock().unwrap();
    let sent: Vec<&str> = log
        .iter()
        .map(|s| s.text.split([' ', '{']).next().unwrap_or(""))
        .collect();
    assert_eq!(sent, ["Tversion", "Tattach", "Tstat"], "{sent:?}");
    assert!(log.iter().all(|s| !s.failed));
}

/// A server with stock Inferno's fault: at an msize above 65544 its room
/// for an `Rstat` wraps in 16 bits and every stat fails. The mount finds
/// that on its stat of the root, starts the conversation again at 65544,
/// and everything then works, stats too.
#[test]
fn a_server_that_cannot_stat_at_64k_is_mounted_smaller() {
    let (mut p, _) = setup();
    let log = Arc::new(Mutex::new(Vec::new()));
    let [srv, cli] = p.pipe().unwrap();
    let fd = p.fgrp.get(srv).unwrap();
    p.close(srv).unwrap();
    let seen = log.clone();
    thread::spawn(move || {
        let mut fs = One {
            fids: Default::default(),
        };
        let mut msize = MAX_MSIZE;
        while let Ok(buf) = read_msg(&fd, msize) {
            let msg = Msg::decode(&buf, msize).unwrap();
            let short = matches!(msg.fcall, purg_styx::Fcall::Tstat { .. }) && msize > 65544;
            let reply = if short {
                Msg {
                    tag: msg.tag,
                    fcall: purg_styx::Fcall::Rerror {
                        ename: error::ESHORTSTAT,
                    },
                }
                .encode()
                .unwrap()
            } else {
                // A version exchange starts afresh: the fids are gone.
                if matches!(msg.fcall, purg_styx::Fcall::Tversion { .. }) {
                    fs.fids.clear();
                }
                dispatch(&mut fs, &msg, &mut msize)
            };
            seen.lock()
                .unwrap()
                .push((format!("{:?}", msg.fcall), short, msize));
            if write_full(&fd, &reply).is_err() {
                break;
            }
        }
    });
    p.mount(cli, -1, "#R/mnt", MREPL | MNOAUTH, "").unwrap();
    p.close(cli).unwrap();
    // Stats work, of the root and of a file, and so does a read.
    assert!(p.stat("#R/mnt").unwrap().qid.is_dir());
    assert_eq!(p.stat("#R/mnt/f").unwrap().name, "f");
    let fd = p.open("#R/mnt", mode::OREAD).unwrap();
    let mut buf = [0u8; 512];
    assert!(p.read(fd, &mut buf).unwrap() > 0);
    let log = log.lock().unwrap();
    let kinds: Vec<&str> = log
        .iter()
        .take(5)
        .map(|s| s.0.split([' ', '{']).next().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        ["Tversion", "Tattach", "Tstat", "Tversion", "Tattach"],
        "{kinds:?}"
    );
    // One stat was refused, the probe; and the second version agreed on
    // the smaller size, which every later message was sent under.
    assert_eq!(log.iter().filter(|s| s.1).count(), 1);
    assert!(log[3].0.contains("65544"), "{}", log[3].0);
    assert!(log[4..].iter().all(|s| s.2 == 65544));
}
