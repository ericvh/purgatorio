//! lockfs [-A] [-a alg]... [-p addr] dir [mountpoint]: serve the files of
//! `dir` with locks: a file may be open by any number of readers or by one
//! writer. An open that would break that waits until the file is free.
//! The tree is mounted on `mountpoint` (default `dir` itself). With `-p`,
//! lockfs instead announces `addr` and exports the locked tree to each
//! client that connects, so all of them share the locks; `-A` exports
//! without authentication. `-a` names an algorithm for the connection; the
//! only one here is `none`.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};

use purg_styx::server::dispatch;
use purg_styx::{Fcall, MAX_MSIZE, Msg};
use purgatorio::abi::mount::{MCREATE, MREPL};
use purgatorio::abi::{mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, DMDIR, Dir, Error};
use styxserv::pass::{Pass, Policy};

const USAGE: &str = "lockfs [-A] [-a alg]... [-p addr] dir [mountpoint]";

/// The `Rerror` text of an open that has to wait; never sent.
const WAIT: &str = "\u{0}lockfs: wait";

#[derive(Default)]
struct Lock {
    readers: usize,
    writer: bool,
}

#[derive(Default)]
struct Locks {
    files: BTreeMap<u64, Lock>,
    /// The file and the kind of lock each open fid holds.
    held: BTreeMap<u32, (u64, bool)>,
}

impl Policy for Locks {
    fn open(&mut self, fid: u32, d: &Dir, m: u8) -> Result<(), String> {
        if d.mode & DMDIR != 0 {
            return Ok(());
        }
        let write = m & mode::ACCESS == mode::OWRITE
            || m & mode::ACCESS == mode::ORDWR
            || m & mode::OTRUNC != 0;
        let l = self.files.entry(d.qid.path).or_default();
        if l.writer || (write && l.readers > 0) {
            return Err(String::from(WAIT));
        }
        if write {
            l.writer = true;
        } else {
            l.readers += 1;
        }
        self.held.insert(fid, (d.qid.path, write));
        Ok(())
    }

    fn close(&mut self, fid: u32) {
        let Some((path, write)) = self.held.remove(&fid) else {
            return;
        };
        if let Some(l) = self.files.get_mut(&path) {
            if write {
                l.writer = false;
            } else {
                l.readers -= 1;
            }
            if !l.writer && l.readers == 0 {
                self.files.remove(&path);
            }
        }
    }
}

fn waits(reply: &[u8]) -> bool {
    matches!(
        Msg::decode(reply, u32::MAX).map(|m| m.fcall),
        Ok(Fcall::Rerror { ename: WAIT })
    )
}

/// Speaks 9P on fd 0. Opens that must wait are kept, in order, and tried
/// again whenever a fid is clunked or removed; a flush forgets one.
fn serve(dir: &str) -> Result<(), Error> {
    // SAFETY: fd 0 is the conversation this prog was started with.
    let mut io = unsafe { File::from_raw_fd(0) };
    let mut fs = Pass::new(dir, Locks::default());
    let mut msize = MAX_MSIZE;
    let mut waiting: VecDeque<(u16, Vec<u8>)> = VecDeque::new();
    loop {
        let Some(buf) = styxserv::read_msg(&mut io)? else {
            return Ok(());
        };
        let msg = Msg::decode(&buf, msize).map_err(|e| Error(e.to_string()))?;
        if let Fcall::Tflush { oldtag } = msg.fcall {
            waiting.retain(|(t, _)| *t != oldtag);
        }
        let reply = dispatch(&mut fs, &msg, &mut msize);
        if waits(&reply) {
            waiting.push_back((msg.tag, buf.clone()));
            continue;
        }
        io.write_all(&reply)?;
        if !matches!(msg.fcall, Fcall::Tclunk { .. } | Fcall::Tremove { .. }) {
            continue;
        }
        let mut still = VecDeque::new();
        while let Some((tag, b)) = waiting.pop_front() {
            let m = Msg::decode(&b, msize).map_err(|e| Error(e.to_string()))?;
            let reply = dispatch(&mut fs, &m, &mut msize);
            if waits(&reply) {
                still.push_back((tag, b));
            } else {
                io.write_all(&reply)?;
            }
        }
        waiting = still;
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let dir = args.get(2).ok_or_else(|| Error::from(USAGE))?;
        return serve(dir);
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut noauth, mut addr) = (false, None);
    while let Some(c) = arg.opt() {
        match c {
            'A' => noauth = true,
            'a' => {
                let alg = arg.earg()?;
                if alg != "none" {
                    return Err(Error(format!("unknown algorithm {alg}")));
                }
            }
            'p' => addr = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    let (dir, mtpt) = match rest.as_slice() {
        [d] => (d, d),
        [d, m] => (d, m),
        _ => return Err(Error(format!("usage: {USAGE}"))),
    };
    let dir = styxserv::abs(dir);
    let d = purgatorio::stat(&dir).map_err(|e| Error(format!("cannot stat {dir}: {e}")))?;
    if d.mode & DMDIR == 0 {
        return Err(Error(format!("{dir}: {}", purgatorio::abi::error::ENOTDIR)));
    }
    let Some(addr) = addr else {
        // The server child opens `dir` before this mount covers it.
        return styxserv::bg::mount(
            "/bin/lockfs.wasm",
            &[String::from("lockfs"), String::from("--serve"), dir],
            mtpt,
            MREPL | MCREATE,
        )
        .map_err(|e| Error(format!("cannot mount on {mtpt}: {e}")));
    };
    // Serve the network from a namespace of its own, in which the locked
    // tree covers `dir`; every client is exported that one tree.
    purgatorio::pctl(pctl::FORKNS, &[])?;
    styxserv::bg::mount(
        "/bin/lockfs.wasm",
        &[String::from("lockfs"), String::from("--serve"), dir.clone()],
        &dir,
        MREPL | MCREATE,
    )
    .map_err(|e| Error(format!("cannot mount on {dir}: {e}")))?;
    let ann = dial::announce(&addr).map_err(|e| Error(format!("cannot announce {addr}: {e}")))?;
    let mut argv = vec![String::from("export")];
    if noauth {
        argv.push(String::from("-A"));
    }
    argv.push(dir);
    loop {
        let conn = dial::listen(&ann.dir).map_err(|e| Error(format!("listen: {e}")))?;
        let Some(data) = conn.data.as_ref() else {
            continue;
        };
        purgatorio::spawn(
            "/bin/export.wasm",
            &argv,
            pctl::NEWFD,
            &[(data.as_raw_fd(), 0), (2, 2)],
        )?;
    }
}
