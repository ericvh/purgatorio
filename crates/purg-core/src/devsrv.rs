//! `#s`: the file2chan registry. Each file's reads and writes are delivered
//! as messages on kernel channels to the prog that created it, which replies
//! on a channel named in the request.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use purg_abi::error;
use purg_abi::fio::{self, pack_read, pack_write_hdr};
use purg_abi::mode;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};
use crate::kchan::{KChan, KMsg, Kgrp};
use crate::prog::Proc;

const QROOT: u64 = 0;

struct SrvFile {
    id: u64,
    name: String,
    read: Arc<KChan>,
    write: Arc<KChan>,
    /// The pid and channel table of the prog that serves the file. Not
    /// the `Proc`: that would keep the server's fds and namespace alive
    /// for as long as the file is listed.
    pid: u32,
    server: Arc<Kgrp>,
    next_fid: AtomicU32,
}

/// The `#s` device.
pub struct DevSrv {
    user: String,
    next: AtomicU64,
    files: Mutex<BTreeMap<u64, Arc<SrvFile>>>,
    by_name: Mutex<BTreeMap<String, u64>>,
}

impl DevSrv {
    pub fn new(user: &str) -> DevSrv {
        DevSrv {
            user: String::from(user),
            next: AtomicU64::new(1),
            files: Mutex::new(BTreeMap::new()),
            by_name: Mutex::new(BTreeMap::new()),
        }
    }

    /// A walk name that does not collide with files already in `#s`.
    pub fn fresh_name(&self) -> String {
        alloc::format!("s{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Creates `name` and returns the server's read and write channel handles.
    pub fn add(&self, proc: &Arc<Proc>, name: &str) -> Result<(i32, i32)> {
        if name.is_empty() || name.contains('/') {
            return Err(Error::new(error::EBADARG));
        }
        if self.by_name.lock().contains_key(name) {
            return Err(Error::new(error::EEXIST));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let read = Arc::new(KChan::new(16));
        let write = Arc::new(KChan::new(16));
        let rh = proc.kgrp.alloc(read.clone())?;
        let wh = match proc.kgrp.alloc(write.clone()) {
            Ok(h) => h,
            Err(e) => {
                let _ = proc.kgrp.close(rh);
                return Err(e);
            }
        };
        let file = Arc::new(SrvFile {
            id,
            name: String::from(name),
            read,
            write,
            pid: proc.pid,
            server: proc.kgrp.clone(),
            next_fid: AtomicU32::new(1),
        });
        self.files.lock().insert(id, file);
        self.by_name.lock().insert(String::from(name), id);
        Ok((rh, wh))
    }

    /// Removes the files prog `pid` serves, when it exits, and closes
    /// their channels: a client's read or write then fails with `i/o on
    /// hungup channel` instead of waiting for an answer that cannot come,
    /// and the name is free again. (Inferno removes a file2chan file when
    /// its server's channels are collected.)
    pub fn remove_server(&self, pid: u32) {
        let gone: Vec<Arc<SrvFile>> = {
            let mut files = self.files.lock();
            let ids: Vec<u64> = files
                .values()
                .filter(|f| f.pid == pid)
                .map(|f| f.id)
                .collect();
            ids.iter().filter_map(|id| files.remove(id)).collect()
        };
        if gone.is_empty() {
            return;
        }
        let mut names = self.by_name.lock();
        for f in &gone {
            names.remove(&f.name);
            f.read.close();
            f.write.close();
        }
    }

    fn file(&self, c: &Chan) -> Result<Arc<SrvFile>> {
        self.files
            .lock()
            .get(&c.qid.path)
            .cloned()
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn dir(&self, id: u64, name: &str, dir: bool) -> Dir {
        Dir {
            dtype: 's' as u16,
            qid: Qid {
                path: id,
                vers: 0,
                qtype: if dir { QTDIR } else { QTFILE },
            },
            mode: if dir { DMDIR | 0o555 } else { 0o666 },
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn exchange(
        &self,
        file: &SrvFile,
        fid: u32,
        write: bool,
        buf: &mut [u8],
        data: &[u8],
        offset: u64,
    ) -> Result<usize> {
        let reply_chan = Arc::new(KChan::new(1));
        let handle = file.server.alloc(reply_chan.clone())?;
        let bytes = if write {
            let mut v = pack_write_hdr(offset, fid, handle).to_vec();
            if data.is_empty() && offset == u64::MAX {
                v[0] = fio::TRUNC;
            } else {
                v.extend_from_slice(data);
            }
            v
        } else {
            pack_read(offset, buf.len() as u32, fid, handle).to_vec()
        };
        let sent = if write {
            file.write.send(KMsg {
                bytes,
                caps: Vec::new(),
            })
        } else {
            file.read.send(KMsg {
                bytes,
                caps: Vec::new(),
            })
        };
        if let Err(e) = sent {
            let _ = file.server.close(handle);
            return Err(e);
        }
        let rep = match reply_chan.recv() {
            Ok(Some(m)) => m,
            Ok(None) => {
                let _ = file.server.close(handle);
                return Err(Error::new(error::EHUNGUP));
            }
            Err(e) => {
                let _ = file.server.close(handle);
                return Err(e);
            }
        };
        let _ = file.server.close(handle);
        if rep.bytes.is_empty() {
            return Err(Error::new(error::EIO));
        }
        match rep.bytes[0] {
            fio::OK_READ if !write => {
                let n = buf.len().min(rep.bytes.len() - 1);
                buf[..n].copy_from_slice(&rep.bytes[1..1 + n]);
                Ok(n)
            }
            fio::OK_WRITE if write && rep.bytes.len() >= 5 => {
                let n = u32::from_le_bytes(rep.bytes[1..5].try_into().unwrap_or([0; 4])) as usize;
                Ok(n.min(data.len()))
            }
            fio::ERR => {
                let msg = core::str::from_utf8(&rep.bytes[1..]).unwrap_or(error::EIO);
                Err(Error::from(String::from(msg)))
            }
            _ => Err(Error::new(error::EIO)),
        }
    }
}

impl Dev for DevSrv {
    fn dc(&self) -> char {
        's'
    }

    fn name(&self) -> &'static str {
        "srv"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QROOT,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        let id = *self
            .by_name
            .lock()
            .get(name)
            .ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: Qid {
                path: id,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        if c.qid.path == QROOT {
            return Ok(self.dir(QROOT, "#s", true));
        }
        let f = self.file(c)?;
        Ok(self.dir(f.id, &f.name, false))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path != QROOT {
            let f = self.file(c)?;
            c.qid.vers = f.next_fid.fetch_add(1, Ordering::Relaxed);
            if omode & mode::OTRUNC != 0 {
                self.exchange(&f, c.qid.vers, true, &mut [], &[], u64::MAX)?;
            }
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path == QROOT {
            return Err(Error::new(error::EISDIR));
        }
        let f = self.file(c)?;
        self.exchange(&f, c.qid.vers, false, buf, &[], offset)
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        if c.qid.path != QROOT {
            return Err(Error::new(error::ENOTDIR));
        }
        let files = self.files.lock();
        Ok(files
            .values()
            .map(|f| self.dir(f.id, &f.name, false))
            .collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        if c.qid.path == QROOT {
            return Err(Error::new(error::EISDIR));
        }
        let f = self.file(c)?;
        self.exchange(&f, c.qid.vers, true, &mut [], buf, offset)
    }
}
