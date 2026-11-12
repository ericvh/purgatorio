//! `#c`: the console device, over [`Platform::console`].
//!
//! `cons` reads a line at a time unless `rawon` was written to `consctl`.
//! A console that is a bare byte stream gets its echo and line editing
//! from [`crate::linedisc`]; the hosted console's terminal does that
//! itself, so there the lines are only split. An interrupt-driven console
//! ([`purg_platform::Console::interrupt_driven`]) is read by the `cons`
//! kproc, started when the device table is built, so that typing is
//! echoed as it arrives and the interrupt key (^C) acts at once: it posts
//! the `interrupt` note to the console's foreground process group, the
//! group of the prog that last read `cons` (`docs/abi.md`, "Interrupts").
//!
//! `user` reads as the calling prog's user. The host owner may write a
//! name to it to become that user (`runas`); anyone else gets
//! `permission denied`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use purg_abi::{error, mode};
use purg_platform::Platform;
use purg_platform::sync::Mutex;

use crate::qio::Queue;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, DevTab, Walked, read_str};
use crate::error::{Error, Result};
use crate::linedisc::ConsReader;
use crate::prog::Procs;

const QDIR: u64 = 0;

/// `(name, qid.path, perm)` for each file, in listing order.
const FILES: &[(&str, u64, u32)] = &[
    ("cons", 1, 0o660),
    ("consctl", 8, 0o220),
    ("drivers", 2, 0o444),
    ("jit", 12, 0o444),
    ("keyboard", 14, 0o444),
    ("kprint", 10, 0o660),
    ("memory", 13, 0o444),
    ("null", 3, 0o666),
    ("pid", 4, 0o444),
    ("random", 9, 0o444),
    ("sysctl", 11, 0o666),
    ("sysname", 5, 0o444),
    ("time", 6, 0o444),
    ("user", 7, 0o666),
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
/// Only with a display ([`Platform::screen`]).
const QKEYBOARD: u64 = 14;

/// Input from a console with its own terminal, delivered a line at a time
/// as Inferno's cons does.
#[derive(Default)]
struct LineBuf {
    pending: Vec<u8>,
    eof: bool,
}

pub struct DevCons {
    platform: Arc<dyn Platform>,
    procs: Arc<Procs>,
    drivers: Mutex<String>,
    input: Mutex<LineBuf>,
    /// Holds the raw flag, and reads a console that wants the line
    /// discipline.
    reader: Arc<ConsReader>,
    /// The console's foreground process group: that of the prog that
    /// last read `cons`; 0 before any did.
    foreground: Arc<AtomicU32>,
    kprint: Queue,
    /// The display whose keys `keyboard` reads, if there is one.
    screen: Option<Arc<dyn purg_platform::screen::Screen>>,
}

impl DevCons {
    pub fn new(platform: Arc<dyn Platform>, procs: Arc<Procs>) -> DevCons {
        let screen = platform.screen();
        DevCons {
            platform,
            procs,
            drivers: Mutex::new(String::new()),
            input: Mutex::new(LineBuf::default()),
            reader: Arc::new(ConsReader::new()),
            foreground: Arc::new(AtomicU32::new(0)),
            kprint: Queue::new(64 * 1024),
            screen,
        }
    }

    /// The files this console has: `keyboard` only with a display.
    fn files(&self) -> impl Iterator<Item = &'static (&'static str, u64, u32)> + '_ {
        FILES
            .iter()
            .filter(|f| f.1 != QKEYBOARD || self.screen.is_some())
    }

    /// Reads typed characters from the display, waiting for one.
    fn read_keyboard(&self, buf: &mut [u8]) -> Result<usize> {
        let Some(screen) = &self.screen else {
            return Err(Error::new(error::ENONEXIST));
        };
        let input = screen.input();
        crate::block::park(&input.rendez, || input.has_keys())?;
        Ok(input.take_keys(buf))
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
        let cons = self.platform.console();
        if let Ok(p) = self.procs.current() {
            self.foreground.store(p.group(), Ordering::Release);
        }
        if cons.line_discipline() {
            return self.reader.read(cons, buf);
        }
        if self.reader.is_raw() {
            return cons.read(buf).map_err(Error::from);
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
        drop(s);
        let cons = self.platform.console();
        if !cons.line_discipline() {
            return;
        }
        let (procs, fg) = (self.procs.clone(), self.foreground.clone());
        self.reader
            .set_interrupt(Box::new(move || match fg.load(Ordering::Acquire) {
                0 => {}
                g => procs.interrupt_group(g),
            }));
        if cons.interrupt_driven() {
            let (reader, platform) = (self.reader.clone(), self.platform.clone());
            self.reader.set_pumped(true);
            let pump = Box::new(move || reader.pump(platform.console()));
            // Without a kproc the reader runs the discipline.
            if self.platform.spawn_kproc("cons", pump).is_err() {
                self.reader.set_pumped(false);
            }
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
        let f = self.files().find(|f| f.0 == name);
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
        let writable = matches!(
            c.qid.path,
            QCONS | QNULL | QCONSCTL | QKPRINT | QSYSCTL | QUSER
        );
        if omode & mode::ACCESS != mode::OREAD && !writable {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let text = match c.qid.path {
            QCONS => return self.read_cons(buf),
            QKEYBOARD => return self.read_keyboard(buf),
            QNULL => return Ok(0),
            QRANDOM => {
                self.platform.entropy(buf);
                return Ok(buf.len());
            }
            QKPRINT => return self.kprint.read(buf),
            QDRIVERS => self.drivers.lock().clone(),
            QPID => format!("{}", self.platform.current_pid()),
            QSYSNAME => self.platform.sysname(),
            QUSER => self.procs.current_user(),
            QTIME => format!("{}", self.platform.now_ns() / 1000),
            QJIT => String::from("0\n"),
            // Inferno's format: one line per pool.
            QMEMORY => {
                let mut text = String::new();
                for p in self.platform.pools() {
                    text.push_str(&format!(
                        "{:11} {:11} {:11} {:11} {:11} {:11} {:11} {}\n",
                        p.cursize, p.maxsize, p.hw, p.nalloc, p.nfree, p.nbrk, p.max, p.name
                    ));
                }
                text
            }
            QSYSCTL => String::from("purgatorio\n"),
            _ => return Err(Error::new(error::EISDIR)),
        };
        Ok(read_str(text.as_bytes(), buf, offset))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(self.files().map(|f| self.file_dir(f.1)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        match c.qid.path {
            QUSER => {
                if !self.procs.is_owner() {
                    return Err(Error::new(error::EPERM));
                }
                let name = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
                let name = name.strip_suffix('\n').unwrap_or(name);
                if offset != 0 {
                    return Err(Error::new(error::EBADARG));
                }
                let p = self.procs.current()?;
                self.procs.set_user(&p, name)?;
                Ok(buf.len())
            }
            QCONS => {
                self.reader.output(buf);
                self.platform.console().write(buf).map_err(Error::from)
            }
            QNULL => Ok(buf.len()),
            QKPRINT | QSYSCTL => self.kprint.write(buf),
            QCONSCTL => {
                let cmd = core::str::from_utf8(buf)
                    .map_err(|_| Error::new(error::EBADARG))?
                    .trim();
                match cmd {
                    "rawon" => {
                        self.reader.set_raw(true);
                        self.platform.console().set_raw(true)?;
                    }
                    "rawoff" => {
                        self.reader.set_raw(false);
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
