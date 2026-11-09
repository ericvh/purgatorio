//! `#c`: the console device, over [`Platform::console`].

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::Platform;
use purg_platform::sync::Mutex;

use crate::qio::Queue;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, DevTab, Walked, read_str};
use crate::error::{Error, Result};

const QDIR: u64 = 0;

/// `(name, qid.path, perm)` for each file, in listing order.
const FILES: &[(&str, u64, u32)] = &[
    ("cons", 1, 0o660),
    ("consctl", 8, 0o220),
    ("drivers", 2, 0o444),
    ("jit", 12, 0o444),
    ("kprint", 10, 0o660),
    ("memory", 13, 0o444),
    ("null", 3, 0o666),
    ("pid", 4, 0o444),
    ("random", 9, 0o444),
    ("sysctl", 11, 0o666),
    ("sysname", 5, 0o444),
    ("time", 6, 0o444),
    ("user", 7, 0o444),
];
const QCONS: u64 = 1;
const QDRIVERS: u64 = 2;
const QNULL: u64 = 3;
const QPID: u64 = 4;
const QSYSNAME: u64 = 5;
const QTIME: u64 = 6;
const QUSER: u64 = 7;
const QCONSCTL: u64 = 8;
const QRANDOM: u64 = 9;
const QKPRINT: u64 = 10;
const QSYSCTL: u64 = 11;
const QJIT: u64 = 12;
const QMEMORY: u64 = 13;

/// Console input, delivered a line at a time as Inferno's cons does.
#[derive(Default)]
struct LineBuf {
    pending: Vec<u8>,
    eof: bool,
}

pub struct DevCons {
    platform: Arc<dyn Platform>,
    drivers: Mutex<String>,
    input: Mutex<LineBuf>,
    raw: core::sync::atomic::AtomicBool,
    kprint: Queue,
}

impl DevCons {
    pub fn new(platform: Arc<dyn Platform>) -> DevCons {
        DevCons {
            platform,
            drivers: Mutex::new(String::new()),
            input: Mutex::new(LineBuf::default()),
            raw: core::sync::atomic::AtomicBool::new(false),
            kprint: Queue::new(64 * 1024),
        }
    }

    fn file_dir(&self, path: u64) -> Dir {
        let user = self.platform.user();
        let (name, qtype, perm) = match FILES.iter().find(|f| f.1 == path) {
            Some(&(n, _, p)) => (n, QTFILE, p),
            None => (".", QTDIR, DMDIR | 0o555),
        };
        Dir {
            dtype: 'c' as u16,
            dev: 0,
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            mode: perm,
            name: String::from(name),
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }
    }

    fn read_cons(&self, buf: &mut [u8]) -> Result<usize> {
        if self.raw.load(core::sync::atomic::Ordering::Acquire) {
            return self.platform.console().read(buf).map_err(Error::from);
        }
        let mut input = self.input.lock();
        while !input.eof && !input.pending.contains(&b'\n') {
            let mut chunk = [0u8; 512];
            let n = self.platform.console().read(&mut chunk)?;
            if n == 0 {
                input.eof = true;
            } else {
                input.pending.extend_from_slice(&chunk[..n]);
            }
        }
        let line_end = match input.pending.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => input.pending.len(),
        };
        let n = buf.len().min(line_end);
        buf[..n].copy_from_slice(&input.pending[..n]);
        input.pending.drain(..n);
        Ok(n)
    }
}

impl Dev for DevCons {
    fn dc(&self) -> char {
        'c'
    }

    fn name(&self) -> &'static str {
        "cons"
    }

    fn init(&self, tab: &DevTab) {
        let mut s = self.drivers.lock();
        for d in tab.iter() {
            s.push_str(&format!("#{} {}\n", d.dc(), d.name()));
        }
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Qid {
                path: QDIR,
                vers: 0,
                qtype: QTDIR,
            },
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        let f = FILES.iter().find(|f| f.0 == name);
        let f = f.ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: Qid {
                path: f.1,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.file_dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let writable = matches!(c.qid.path, QCONS | QNULL | QCONSCTL | QKPRINT | QSYSCTL);
        if omode & mode::ACCESS != mode::OREAD && !writable {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let text = match c.qid.path {
            QCONS => return self.read_cons(buf),
            QNULL => return Ok(0),
            QRANDOM => {
                self.platform.entropy(buf);
                return Ok(buf.len());
            }
            QKPRINT => return self.kprint.read(buf),
            QDRIVERS => self.drivers.lock().clone(),
            QPID => format!("{}", self.platform.current_pid()),
            QSYSNAME => self.platform.sysname(),
            QUSER => self.platform.user(),
            QTIME => format!("{}", self.platform.now_ns() / 1000),
            QJIT => String::from("0\n"),
            QMEMORY => {
                let (used, total) = self.platform.memory();
                format!("{used} {total}\n")
            }
            QSYSCTL => String::from("purgatorio\n"),
            _ => return Err(Error::new(error::EISDIR)),
        };
        Ok(read_str(text.as_bytes(), buf, offset))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(FILES.iter().map(|f| self.file_dir(f.1)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        match c.qid.path {
            QCONS => self.platform.console().write(buf).map_err(Error::from),
            QNULL => Ok(buf.len()),
            QKPRINT | QSYSCTL => self.kprint.write(buf),
            QCONSCTL => {
                let cmd = core::str::from_utf8(buf)
                    .map_err(|_| Error::new(error::EBADARG))?
                    .trim();
                match cmd {
                    "rawon" => {
                        self.raw.store(true, core::sync::atomic::Ordering::Release);
                        self.platform.console().set_raw(true)?;
                    }
                    "rawoff" => {
                        self.raw.store(false, core::sync::atomic::Ordering::Release);
                        self.platform.console().set_raw(false)?;
                    }
                    _ => return Err(Error::new(error::EBADARG)),
                }
                Ok(buf.len())
            }
            _ => Err(Error::new(error::EPERM)),
        }
    }
}
