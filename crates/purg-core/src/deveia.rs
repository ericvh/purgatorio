//! `#t`: serial ports, as in Inferno's native kernels' eia device.
//!
//! Port `n` has three files: `eian` (the byte stream), `eianctl` and
//! `eianstatus`. Each port is a [`Console`] byte stream; on the kernel,
//! port 0 is the UART that is also the console, shared as Inferno's
//! native ports share a serial console.
//!
//! `eianctl` takes Inferno's commands, several to a write, separated by
//! white space: `bn` baud rate, `ln` bits per character (5 to 8), `pc`
//! parity (`n`, `e`, `o`, `m` or `s`), `sn` stop bits (1 or 2), `dn` DTR,
//! `rn` RTS, `mn` CTS flow control, `xn` XON/XOFF, `cn` and `en` hang up on
//! DCD or DSR, and `in` the FIFO. They are recorded and reported by
//! `eianstatus`; a port with no line settings (an emulated UART) ignores
//! them. `f`, `h`, `kn`, `n`, `qn` and `wn` (flush, hang up, break,
//! non-blocking, queue size, timer) are accepted and do nothing. Anything
//! else fails with `bad arg in system call`.
//!
//! `eianstatus` reads as Inferno's: one line of settings,
//! `b115200 c0 d0 e0 l8 m0 pn r1 s1 i1`, where `d` is the DSR input (0:
//! there are no modem inputs), then a line naming the output signals that
//! are on (` dtr rts`).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::Console;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QDIR: u64 = 0;
const DATA: u64 = 0;
const CTL: u64 = 1;
const STATUS: u64 = 2;

fn qpath(port: usize, kind: u64) -> u64 {
    1 + port as u64 * 3 + kind
}

/// `(port, kind)` of a file's qid path.
fn split(path: u64) -> (usize, u64) {
    let p = path - 1;
    ((p / 3) as usize, p % 3)
}

/// A port's line settings, as `eianctl` sets them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub baud: u32,
    pub bits: u32,
    pub parity: char,
    pub stop: u32,
    pub dtr: bool,
    pub rts: bool,
    pub cts_flow: bool,
    pub xonxoff: bool,
    pub hup_dcd: bool,
    pub hup_dsr: bool,
    pub fifo: u32,
}

impl Default for Line {
    fn default() -> Line {
        Line {
            baud: 115_200,
            bits: 8,
            parity: 'n',
            stop: 1,
            dtr: true,
            rts: true,
            cts_flow: false,
            xonxoff: false,
            hup_dcd: false,
            hup_dsr: false,
            fifo: 1,
        }
    }
}

impl Line {
    /// Applies one write to `eianctl`.
    pub fn ctl(&mut self, cmds: &str) -> Result<()> {
        let bad = || Error::new(error::EBADARG);
        let mut next = self.clone();
        for cmd in cmds.split_ascii_whitespace() {
            let mut chars = cmd.chars();
            let Some(c) = chars.next() else { continue };
            let arg = chars.as_str();
            let num = || arg.parse::<u32>().map_err(|_| bad());
            let flag = || match num()? {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(bad()),
            };
            match c.to_ascii_lowercase() {
                'b' => next.baud = num().ok().filter(|&b| b > 0).ok_or_else(bad)?,
                'l' => next.bits = num().ok().filter(|b| (5..=8).contains(b)).ok_or_else(bad)?,
                'p' => {
                    next.parity = match arg {
                        "n" | "e" | "o" | "m" | "s" => arg.chars().next().unwrap_or('n'),
                        _ => return Err(bad()),
                    }
                }
                's' => next.stop = num().ok().filter(|s| (1..=2).contains(s)).ok_or_else(bad)?,
                'd' => next.dtr = flag()?,
                'r' => next.rts = flag()?,
                'm' => next.cts_flow = flag()?,
                'x' => next.xonxoff = flag()?,
                'c' => next.hup_dcd = flag()?,
                'e' => next.hup_dsr = flag()?,
                'i' => next.fifo = num()?,
                'f' | 'h' | 'k' | 'n' | 'q' | 'w' => {}
                _ => return Err(bad()),
            }
        }
        *self = next;
        Ok(())
    }

    /// What `eianstatus` reads.
    pub fn status(&self) -> String {
        let b = |v: bool| u8::from(v);
        let mut s = format!(
            "b{} c{} d0 e{} l{} m{} p{} r{} s{} i{}\n",
            self.baud,
            b(self.hup_dcd),
            b(self.hup_dsr),
            self.bits,
            b(self.cts_flow),
            self.parity,
            b(self.rts),
            self.stop,
            self.fifo
        );
        if self.dtr {
            s.push_str(" dtr");
        }
        if self.rts {
            s.push_str(" rts");
        }
        s.push('\n');
        s
    }
}

struct Port {
    io: Arc<dyn Console>,
    line: Mutex<Line>,
}

pub struct DevEia {
    ports: Vec<Port>,
    user: String,
}

impl DevEia {
    /// Serves `ports[n]` as `eian`.
    pub fn new(ports: Vec<Arc<dyn Console>>, user: String) -> DevEia {
        DevEia {
            ports: ports
                .into_iter()
                .map(|io| Port {
                    io,
                    line: Mutex::new(Line::default()),
                })
                .collect(),
            user,
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, qtype, perm) = if path == QDIR {
            (String::from("#t"), QTDIR, DMDIR | 0o555)
        } else {
            let (port, kind) = split(path);
            match kind {
                DATA => (format!("eia{port}"), QTFILE, 0o660),
                CTL => (format!("eia{port}ctl"), QTFILE, 0o660),
                _ => (format!("eia{port}status"), QTFILE, 0o444),
            }
        };
        Dir {
            dtype: 't' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            mode: perm,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn port(&self, path: u64) -> Result<(&Port, u64)> {
        if path == QDIR {
            return Err(Error::new(error::EISDIR));
        }
        let (port, kind) = split(path);
        let p = self.ports.get(port).ok_or(Error::new(error::ENONEXIST))?;
        Ok((p, kind))
    }
}

impl Dev for DevEia {
    fn dc(&self) -> char {
        't'
    }

    fn name(&self) -> &'static str {
        "eia"
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
        let path = (0..self.ports.len())
            .flat_map(|p| [DATA, CTL, STATUS].map(|k| qpath(p, k)))
            .find(|&q| self.dir(q).name == name)
            .ok_or(Error::new(error::ENONEXIST))?;
        Ok(Walked {
            qid: Qid {
                path,
                vers: 0,
                qtype: QTFILE,
            },
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let write = omode & mode::ACCESS != mode::OREAD;
        if c.qid.path == QDIR {
            return if write {
                Err(Error::new(error::EISDIR))
            } else {
                Ok(())
            };
        }
        let (_, kind) = self.port(c.qid.path)?;
        if write && kind == STATUS {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let (port, kind) = self.port(c.qid.path)?;
        match kind {
            DATA => port.io.read(buf).map_err(Error::from),
            CTL => Ok(0),
            _ => {
                let s = port.line.lock().status();
                Ok(read_str(s.as_bytes(), buf, offset))
            }
        }
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok((0..self.ports.len())
            .flat_map(|p| [DATA, CTL, STATUS].map(|k| qpath(p, k)))
            .map(|q| self.dir(q))
            .collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        let (port, kind) = self.port(c.qid.path)?;
        match kind {
            DATA => port.io.write(buf).map_err(Error::from),
            CTL => {
                let cmds = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
                port.line.lock().ctl(cmds)?;
                Ok(buf.len())
            }
            _ => Err(Error::new(error::EPERM)),
        }
    }
}
