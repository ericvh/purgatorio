//! `#^`: the snarf buffer, as Inferno's emu has it.
//!
//! The device holds one file, `snarf`: the text cut and pasted between
//! programs. A read at offset 0 takes a copy of the buffer, and reads
//! further on read that copy, so a reader sees one buffer even while
//! another program replaces it. Writes go to a buffer of the open file's
//! own, appended whatever their offset, and when a file opened for
//! writing is closed, what was written replaces the snarf buffer, even
//! when nothing was (a file opened to read and write, without `OTRUNC`,
//! that nothing was written to leaves it alone). More than [`MAXSNARF`] bytes is an error.
//!
//! When the platform's screen can reach the host's clipboard
//! ([`Screen::snarf_get`], [`Screen::snarf_put`]), the clipboard is the
//! buffer: a read at offset 0 takes the clipboard's text, and a close
//! after writing puts the text there too. Otherwise the buffer is the
//! device's own, shared by every prog.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::screen::Screen;
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QDIR: u64 = 0;
const QSNARF: u64 = 1;

/// The most a write may hold; ours, not Inferno's.
pub const MAXSNARF: usize = 1 << 20;
/// Inferno's `Etoobig`.
const ETOOBIG: &str = "read or write too large";

/// What an open `snarf` holds: the copy read at offset 0, and what has
/// been written.
#[derive(Default)]
struct Open {
    read: Vec<u8>,
    written: Vec<u8>,
}

struct Buffer {
    text: Vec<u8>,
    /// Bumped each time the buffer is replaced: the file's qid version.
    vers: u32,
}

pub struct DevSnarf {
    buf: Mutex<Buffer>,
    host: Option<Arc<dyn Screen>>,
    user: String,
}

impl DevSnarf {
    /// The snarf device; with `host`, a screen whose backend may reach the
    /// host's clipboard.
    pub fn new(host: Option<Arc<dyn Screen>>, user: String) -> DevSnarf {
        DevSnarf {
            buf: Mutex::new(Buffer {
                text: Vec::new(),
                vers: 0,
            }),
            host,
            user,
        }
    }

    fn qid(&self, path: u64) -> Qid {
        Qid {
            path,
            vers: if path == QSNARF {
                self.buf.lock().vers
            } else {
                0
            },
            qtype: if path == QDIR { QTDIR } else { QTFILE },
        }
    }

    fn dir(&self, path: u64) -> Dir {
        let (name, perm, length) = match path {
            QSNARF => ("snarf", 0o666, self.buf.lock().text.len() as u64),
            _ => ("#^", DMDIR | 0o555, 0),
        };
        Dir {
            dtype: '^' as u16,
            qid: self.qid(path),
            mode: perm,
            length,
            name: String::from(name),
            uid: self.user.clone(),
            gid: self.user.clone(),
            ..Dir::default()
        }
    }

    /// The buffer now: the host's clipboard when there is one.
    fn current(&self) -> Vec<u8> {
        if let Some(text) = self.host.as_ref().and_then(|h| h.snarf_get()) {
            let mut b = self.buf.lock();
            if b.text != text.as_bytes() {
                b.text = text.into_bytes();
                b.vers = b.vers.wrapping_add(1);
            }
            return b.text.clone();
        }
        self.buf.lock().text.clone()
    }

    /// Replaces the buffer with `text`.
    fn replace(&self, text: Vec<u8>) {
        if let Some(h) = self.host.as_ref() {
            h.snarf_put(&String::from_utf8_lossy(&text));
        }
        let mut b = self.buf.lock();
        b.text = text;
        b.vers = b.vers.wrapping_add(1);
    }
}

fn with_open<R>(c: &Chan, f: impl FnOnce(&mut Open) -> R) -> Option<R> {
    let mut st = c.state.lock();
    st.as_mut().and_then(|s| s.downcast_mut::<Open>()).map(f)
}

fn writable(omode: u8) -> bool {
    let a = omode & mode::ACCESS;
    a == mode::OWRITE || a == mode::ORDWR
}

impl Dev for DevSnarf {
    fn dc(&self) -> char {
        '^'
    }

    fn name(&self) -> &'static str {
        "snarf"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: self.qid(QDIR),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        if c.qid.path != QDIR {
            return Err(Error::new(error::ENOTDIR));
        }
        if name != "snarf" {
            return Err(Error::new(error::ENONEXIST));
        }
        Ok(Walked {
            qid: self.qid(QSNARF),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dir(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        if c.qid.path == QDIR {
            if omode & mode::ACCESS != mode::OREAD {
                return Err(Error::new(error::EISDIR));
            }
            return Ok(());
        }
        *c.state.lock() = Some(Box::new(Open::default()));
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path != QSNARF {
            return Err(Error::new(error::EISDIR));
        }
        let now = (offset == 0).then(|| self.current());
        with_open(c, |o| {
            if let Some(now) = now {
                o.read = now;
            }
            read_str(&o.read, buf, offset)
        })
        .ok_or(Error::new(error::EBADUSEFD))
    }

    fn dirread(&self, _c: &Chan) -> Result<Vec<Dir>> {
        Ok(alloc::vec![self.dir(QSNARF)])
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.path != QSNARF {
            return Err(Error::new(error::EISDIR));
        }
        with_open(c, |o| {
            if o.written.len() + buf.len() > MAXSNARF {
                return Err(Error::new(ETOOBIG));
            }
            o.written.extend_from_slice(buf);
            Ok(buf.len())
        })
        .ok_or(Error::new(error::EBADUSEFD))?
    }

    fn close(&self, c: &Chan) {
        if c.qid.path != QSNARF || !writable(c.mode) {
            return;
        }
        // A file opened to read and write that was only read leaves the
        // buffer alone.
        let only_read = c.mode & mode::ACCESS == mode::ORDWR && c.mode & mode::OTRUNC == 0;
        if let Some(text) = with_open(c, |o| core::mem::take(&mut o.written))
            && !(only_read && text.is_empty())
        {
            self.replace(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use purg_platform::screen::{Headless, Input};

    fn open(d: &Arc<DevSnarf>, omode: u8) -> Chan {
        let dev: Arc<dyn Dev> = d.clone();
        let root = d.attach("").unwrap();
        let mut c = Chan::new(dev.clone(), 0, root.qid, String::from("#^"), None);
        let w = d.walk(&c, "snarf").unwrap();
        c = Chan::new(dev, 0, w.qid, String::from("#^/snarf"), None);
        d.open(&mut c, omode).unwrap();
        c.mode = omode;
        c.open = true;
        c
    }

    fn read_all(d: &Arc<DevSnarf>) -> Vec<u8> {
        let c = open(d, mode::OREAD);
        let mut out = Vec::new();
        let mut b = [0u8; 3];
        loop {
            let n = d.read(&c, &mut b, out.len() as u64).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&b[..n]);
        }
    }

    fn put(d: &Arc<DevSnarf>, parts: &[&[u8]]) {
        let c = open(d, mode::OWRITE | mode::OTRUNC);
        for p in parts {
            d.write(&c, p, 0).unwrap();
        }
        // Closing the file (dropping the Chan) replaces the buffer.
        drop(c);
    }

    #[test]
    fn write_replaces_on_close() {
        let d = Arc::new(DevSnarf::new(None, String::from("inferno")));
        assert_eq!(read_all(&d), b"");
        put(&d, &[b"hello, ", b"world"]);
        assert_eq!(read_all(&d), b"hello, world");
        // Until the writer closes, readers see the old buffer.
        let w = open(&d, mode::OWRITE);
        d.write(&w, b"new", 0).unwrap();
        assert_eq!(read_all(&d), b"hello, world");
        drop(w);
        assert_eq!(read_all(&d), b"new");
        // Opening for writing and closing empties it.
        let rw = open(&d, mode::ORDWR);
        drop(rw);
        assert_eq!(read_all(&d), b"new");
        put(&d, &[]);
        assert_eq!(read_all(&d), b"");
        let v = d.dir(QSNARF).qid.vers;
        put(&d, &[b"x"]);
        assert_ne!(d.dir(QSNARF).qid.vers, v);
        assert_eq!(d.dir(QSNARF).length, 1);
    }

    #[test]
    fn a_read_keeps_its_copy() {
        let d = Arc::new(DevSnarf::new(None, String::from("inferno")));
        put(&d, &[b"abcdef"]);
        let r = open(&d, mode::OREAD);
        let mut b = [0u8; 2];
        assert_eq!(d.read(&r, &mut b, 0).unwrap(), 2);
        put(&d, &[b"XYZXYZ"]);
        assert_eq!(d.read(&r, &mut b, 2).unwrap(), 2);
        assert_eq!(&b, b"cd");
        // Reading from the start again takes the new buffer.
        assert_eq!(d.read(&r, &mut b, 0).unwrap(), 2);
        assert_eq!(&b, b"XY");
    }

    #[test]
    fn too_much() {
        let d = Arc::new(DevSnarf::new(None, String::from("inferno")));
        let w = open(&d, mode::OWRITE);
        let big = alloc::vec![b'a'; MAXSNARF];
        d.write(&w, &big, 0).unwrap();
        assert_eq!(d.write(&w, b"b", 0).unwrap_err().as_str(), ETOOBIG);
    }

    #[test]
    fn names_and_errors() {
        let d = Arc::new(DevSnarf::new(None, String::from("inferno")));
        let dev: Arc<dyn Dev> = d.clone();
        let root = d.attach("").unwrap();
        let mut c = Chan::new(dev, 0, root.qid, String::from("#^"), None);
        assert_eq!(d.walk(&c, "clip").err().unwrap().as_str(), error::ENONEXIST);
        assert_eq!(
            d.open(&mut c, mode::OWRITE).unwrap_err().as_str(),
            error::EISDIR
        );
        let ents = d.dirread(&c).unwrap();
        assert_eq!(ents.len(), 1);
        assert_eq!(ents[0].name, "snarf");
        assert_eq!(ents[0].mode, 0o666);
    }

    /// A screen whose backend has a clipboard.
    struct Clip {
        screen: Headless,
        text: Mutex<String>,
    }

    impl Screen for Clip {
        fn size(&self) -> (u32, u32) {
            self.screen.size()
        }
        fn flush(&self, r: purg_platform::screen::Rect, p: &[u8], s: usize) {
            self.screen.flush(r, p, s);
        }
        fn input(&self) -> &Input {
            self.screen.input()
        }
        fn snarf_get(&self) -> Option<String> {
            Some(self.text.lock().clone())
        }
        fn snarf_put(&self, s: &str) -> bool {
            *self.text.lock() = String::from(s);
            true
        }
    }

    #[test]
    fn the_host_clipboard() {
        let clip = Arc::new(Clip {
            screen: Headless::new(8, 8),
            text: Mutex::new(String::from("from the host")),
        });
        let d = Arc::new(DevSnarf::new(
            Some(clip.clone() as Arc<dyn Screen>),
            String::from("inferno"),
        ));
        assert_eq!(read_all(&d), b"from the host");
        put(&d, &[b"from inferno"]);
        assert_eq!(*clip.text.lock(), "from inferno");
        *clip.text.lock() = String::from("again");
        assert_eq!(read_all(&d), b"again");
    }
}
