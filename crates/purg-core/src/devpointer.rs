//! `#m`: the pointer (pointer(3)), from a [`Screen`]'s input.
//!
//! A read of `pointer` waits for the pointer to move or a button to
//! change, then returns 49 bytes: `m` and four 12-byte decimal fields, x,
//! y, buttons and milliseconds. Writing `m x y` (the `m` may be left
//! out) moves the pointer there. `cursor` takes a cursor image, which is
//! accepted and not shown yet. `purgatorio.wait` (and `poll_oneoff`) see
//! `pointer` ready to read when an event is queued.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::screen::{Pointer, Screen};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};

const QDIR: u64 = 0;
const QCURSOR: u64 = 1;
const QPOINTER: u64 = 2;
const FILES: &[(&str, u64)] = &[("cursor", QCURSOR), ("pointer", QPOINTER)];

pub struct DevPointer {
    screen: Arc<dyn Screen>,
    user: String,
    /// Milliseconds since an arbitrary point, for written moves.
    clock: Arc<dyn Fn() -> u32 + Send + Sync>,
}

impl DevPointer {
    pub fn new(
        screen: Arc<dyn Screen>,
        user: String,
        clock: Arc<dyn Fn() -> u32 + Send + Sync>,
    ) -> DevPointer {
        DevPointer {
            screen,
            user,
            clock,
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, qtype, perm) = match FILES.iter().find(|f| f.1 == path) {
            Some(f) => (f.0, QTFILE, 0o666),
            None => ("#m", QTDIR, DMDIR | 0o555),
        };
        Dir {
            dtype: 'm' as u16,
            qid: Qid {
                path,
                vers: 0,
                qtype,
            },
            mode: perm,
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }
}

/// The 49-byte form of a pointer state.
pub fn format_pointer(p: &Pointer) -> String {
    format!("m{:>11} {:>11} {:>11} {:>11} ", p.x, p.y, p.buttons, p.msec)
}

/// Parses a write to `pointer`: `m x y`, the `m` optional.
fn parse_move(buf: &[u8]) -> Result<(i32, i32)> {
    let s = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
    let s = s.trim_start().strip_prefix('m').unwrap_or(s);
    let mut f = s.split_ascii_whitespace().map(str::parse::<i32>);
    match (f.next(), f.next()) {
        (Some(Ok(x)), Some(Ok(y))) => Ok((x, y)),
        _ => Err(Error::new(error::EBADARG)),
    }
}

impl Dev for DevPointer {
    fn dc(&self) -> char {
        'm'
    }

    fn name(&self) -> &'static str {
        "pointer"
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
        let f = FILES
            .iter()
            .find(|f| f.0 == name)
            .ok_or(Error::new(error::ENONEXIST))?;
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
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QDIR && omode & mode::ACCESS != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], _offset: u64) -> Result<usize> {
        match c.qid.path {
            QPOINTER => {
                let input = self.screen.input();
                let p = loop {
                    crate::block::park(&input.rendez, || input.has_pointer())?;
                    if let Some(p) = input.take_pointer() {
                        break p;
                    }
                };
                let s = format_pointer(&p);
                let n = buf.len().min(s.len());
                buf[..n].copy_from_slice(&s.as_bytes()[..n]);
                Ok(n)
            }
            QCURSOR => Ok(0),
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(FILES.iter().map(|f| self.dir(f.1)).collect())
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        match c.qid.path {
            QPOINTER => {
                let (x, y) = parse_move(buf)?;
                let input = self.screen.input();
                let last = input.last_pointer();
                input.pointer(Pointer {
                    x,
                    y,
                    buttons: last.buttons,
                    msec: (self.clock)(),
                });
                self.screen.set_pointer(x, y);
                Ok(buf.len())
            }
            QCURSOR => Ok(buf.len()),
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    /// A read of `pointer` waits for an event; everything else is ready.
    fn ready(&self, c: &Chan, write: bool) -> bool {
        write || c.qid.path != QPOINTER || self.screen.input().has_pointer()
    }

    fn watch(&self, c: &Chan, write: bool, w: &purg_platform::sync::Rendez) {
        if !write && c.qid.path == QPOINTER {
            self.screen.input().watch(w);
        }
    }

    fn unwatch(&self, c: &Chan, w: &purg_platform::sync::Rendez) {
        if c.qid.path == QPOINTER {
            self.screen.input().unwatch(w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        let p = Pointer {
            x: 10,
            y: -2,
            buttons: 5,
            msec: 1234,
        };
        let s = format_pointer(&p);
        assert_eq!(s.len(), 49);
        assert_eq!(s, "m         10          -2           5        1234 ");
        assert_eq!(parse_move(b"m 3 4").unwrap(), (3, 4));
        assert_eq!(parse_move(b"30 40\n").unwrap(), (30, 40));
        assert!(parse_move(b"m 3").is_err());
    }

    /// `pointer` is ready to read only with an event queued, and a
    /// watcher is woken by the event.
    #[test]
    fn ready_and_watch() {
        extern crate std;
        use purg_platform::screen::Headless;
        use purg_platform::sync::Rendez;
        let screen = Arc::new(Headless::new(8, 8));
        let d = Arc::new(DevPointer::new(
            screen.clone(),
            String::from("me"),
            Arc::new(|| 0),
        ));
        let qid = |path| Qid {
            path,
            vers: 0,
            qtype: QTFILE,
        };
        let p = Chan::new(d.clone(), 0, qid(QPOINTER), String::from("pointer"), None);
        let c = Chan::new(d.clone(), 0, qid(QCURSOR), String::from("cursor"), None);
        assert!(!d.ready(&p, false));
        assert!(d.ready(&p, true));
        assert!(d.ready(&c, false));
        let w = Rendez::new();
        d.watch(&p, false, &w);
        let woken = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let (w2, woken2, s2) = (w.clone(), woken.clone(), screen.clone());
        let t = std::thread::spawn(move || {
            w2.sleep(|| s2.input().has_pointer());
            woken2.store(true, core::sync::atomic::Ordering::Release);
        });
        screen.input().pointer(Pointer {
            x: 1,
            y: 2,
            buttons: 0,
            msec: 3,
        });
        t.join().unwrap();
        assert!(woken.load(core::sync::atomic::Ordering::Acquire));
        assert!(d.ready(&p, false));
        d.unwatch(&p, &w);
    }
}
