//! `#|`: pipes. Each attach makes a new pipe, a directory holding `data`
//! and `data1`; what is written on one end is read from the other. When the
//! last open of an end closes, readers of the other end see end of file and
//! its writers fail with `"i/o on hungup channel"`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use purg_abi::error;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};
use crate::qio::Queue;

/// Bytes a pipe buffers in each direction before writers block.
pub const PIPE_QSIZE: usize = 32 * 1024;

const QDIR: u64 = 0;
const QDATA0: u64 = 1;
const QDATA1: u64 = 2;

struct Pipe {
    id: u64,
    /// `q[k]` is read by end `k` and written by the other end.
    q: [Queue; 2],
    opens: Mutex<[u32; 2]>,
}

pub struct DevPipe {
    next: AtomicU64,
    user: String,
}

impl DevPipe {
    pub fn new(user: &str) -> DevPipe {
        DevPipe {
            next: AtomicU64::new(1),
            user: String::from(user),
        }
    }
}

fn pipe(c: &Chan) -> Result<&Pipe> {
    c.aux::<Pipe>().ok_or(Error::new(error::EBADUSEFD))
}

fn kind(c: &Chan) -> u64 {
    c.qid.path & 3
}

fn end(c: &Chan) -> Option<usize> {
    match kind(c) {
        QDATA0 => Some(0),
        QDATA1 => Some(1),
        _ => None,
    }
}

impl DevPipe {
    fn qid(p: &Pipe, kind: u64) -> Qid {
        Qid {
            path: (p.id << 2) | kind,
            vers: 0,
            qtype: if kind == QDIR { QTDIR } else { QTFILE },
        }
    }

    fn dir(&self, p: &Pipe, kind: u64) -> Dir {
        let (name, mode, length) = match kind {
            QDATA0 => ("data", 0o600, p.q[0].len() as u64),
            QDATA1 => ("data1", 0o600, p.q[1].len() as u64),
            _ => ("#|", DMDIR | 0o500, 0),
        };
        Dir {
            dtype: '|' as u16,
            qid: DevPipe::qid(p, kind),
            mode,
            length,
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }
}

impl Dev for DevPipe {
    fn dc(&self) -> char {
        '|'
    }

    fn name(&self) -> &'static str {
        "pipe"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        let p = Pipe {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            q: [Queue::new(PIPE_QSIZE), Queue::new(PIPE_QSIZE)],
            opens: Mutex::new([0, 0]),
        };
        Ok(Walked {
            qid: DevPipe::qid(&p, QDIR),
            aux: Some(Arc::new(p) as Arc<dyn Any + Send + Sync>),
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if kind(c) != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        let k = match name {
            "data" => QDATA0,
            "data1" => QDATA1,
            _ => return Err(Error::new(error::ENONEXIST)),
        };
        Ok(Walked {
            qid: DevPipe::qid(pipe(c)?, k),
            aux: c.aux.clone(),
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(pipe(c)?, kind(c)))
    }

    fn open(&self, c: &mut Chan, _omode: u8) -> Result<()> {
        if let Some(e) = end(c) {
            pipe(c)?.opens.lock()[e] += 1;
        }
        Ok(())
    }

    fn close(&self, c: &Chan) {
        let (Some(e), Ok(p)) = (end(c), pipe(c)) else {
            return;
        };
        let last = {
            let mut opens = p.opens.lock();
            opens[e] = opens[e].saturating_sub(1);
            opens[e] == 0
        };
        if last {
            p.q[0].hangup();
            p.q[1].hangup();
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], _offset: u64) -> Result<usize> {
        let e = end(c).ok_or(Error::new(error::EISDIR))?;
        pipe(c)?.q[e].read(buf)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let p = pipe(c)?;
        Ok(alloc::vec![self.dir(p, QDATA0), self.dir(p, QDATA1)])
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        let e = end(c).ok_or(Error::new(error::EISDIR))?;
        pipe(c)?.q[1 - e].write(buf)
    }

    fn ready(&self, c: &Chan, write: bool) -> bool {
        let Ok(p) = pipe(c) else {
            return true;
        };
        let Some(e) = end(c) else {
            return true;
        };
        let q = if write { &p.q[1 - e] } else { &p.q[e] };
        if write { q.writable() } else { q.readable() }
    }

    fn watch(&self, c: &Chan, write: bool, w: &purg_platform::sync::Rendez) {
        let Ok(p) = pipe(c) else {
            return;
        };
        let Some(e) = end(c) else {
            return;
        };
        let q = if write { &p.q[1 - e] } else { &p.q[e] };
        q.watch(w);
    }

    fn unwatch(&self, c: &Chan, w: &purg_platform::sync::Rendez) {
        let Ok(p) = pipe(c) else {
            return;
        };
        let Some(e) = end(c) else {
            return;
        };
        p.q[e].unwatch(w);
        p.q[1 - e].unwatch(w);
    }
}
