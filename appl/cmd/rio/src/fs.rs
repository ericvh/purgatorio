//! The files rio serves each window (rio(4)), from its event loop.
//!
//! Each window has a pipe of its own. Its far end is mounted before
//! `/dev` in the namespace of the window's process group (by `rio -W`,
//! see `main.rs`), so the programs in the window see its `cons`,
//! `consctl`, `keyboard`, `pointer`, `label`, `text`, `wctl`, `winid`
//! and `winname` there, with the rest of `/dev` (`draw`, `user`, ...)
//! after them, and `wsys/<id>/` holding the same files of every window.
//! rio answers the messages that come on a window's pipe at once,
//! except reads of `cons`, `pointer` and `keyboard` with nothing to
//! read: those wait, with their tags, until there is something, or a
//! `Tflush` or the clunk of their fid takes them away.

use std::collections::HashMap;
use std::io::{Read, Write};

use purg_styx::{DMDIR, Dir, Fcall, IOHDRSZ, MAX_MSIZE, Msg, QTDIR, QTFILE, Qid};
use purgatorio::Error;
use purgatorio::abi::{error, mode};
use purgatorio::draw::{Point, Rect};

use crate::Rio;

pub const EDELETED: &str = "window deleted";
const EBADWCTL: &str = "unrecognized wctl command";
const EBADRECT: &str = "bad rectangle in wctl request";

/// The files, numbered for qid paths: `(window id << 8) | file`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Q {
    Root = 0,
    Cons,
    Consctl,
    Keyboard,
    Label,
    Pointer,
    Text,
    Wctl,
    Winid,
    Winname,
    Wsys,
    /// `wsys/<id>`.
    Dir,
}

/// A window's files, in its directory and in `wsys/<id>`.
const FILES: &[(&str, Q, u32)] = &[
    ("cons", Q::Cons, 0o600),
    ("consctl", Q::Consctl, 0o200),
    ("keyboard", Q::Keyboard, 0o400),
    ("label", Q::Label, 0o600),
    ("pointer", Q::Pointer, 0o600),
    ("text", Q::Text, 0o400),
    ("wctl", Q::Wctl, 0o600),
    ("winid", Q::Winid, 0o400),
    ("winname", Q::Winname, 0o400),
];

fn is_dir(q: Q) -> bool {
    matches!(q, Q::Root | Q::Wsys | Q::Dir)
}

fn qid(q: Q, win: u32) -> Qid {
    Qid {
        path: (u64::from(win) << 8) | q as u64,
        vers: 0,
        qtype: if is_dir(q) { QTDIR } else { QTFILE },
    }
}

pub struct Fid {
    q: Q,
    win: u32,
    open: Option<u8>,
    /// What a read at offset 0 found, for reads further on.
    snap: Vec<u8>,
    /// The bytes of a rune split between writes.
    carry: Vec<u8>,
}

/// A read waiting for something to read.
pub struct Pending {
    tag: u16,
    fid: u32,
    q: Q,
    win: u32,
    count: u32,
}

/// One window's conversation.
pub struct Session {
    pub io: std::fs::File,
    buf: Vec<u8>,
    msize: u32,
    fids: HashMap<u32, Fid>,
    pub reads: Vec<Pending>,
}

impl Session {
    pub fn new(io: std::fs::File) -> Session {
        Session {
            io,
            buf: Vec::new(),
            msize: MAX_MSIZE,
            fids: HashMap::new(),
            reads: Vec::new(),
        }
    }

    fn send(&mut self, tag: u16, fcall: Fcall<'_>) -> Result<(), Error> {
        let out = Msg { tag, fcall }
            .encode()
            .map_err(|e| Error(e.to_string()))?;
        self.io.write_all(&out)?;
        Ok(())
    }
}

type R<T> = Result<T, String>;

fn err<T>(s: &str) -> R<T> {
    Err(String::from(s))
}

/// The UTF-8 text in `carry` and `data`, keeping a rune cut at the end
/// for the next write.
fn utf8(carry: &mut Vec<u8>, data: &[u8]) -> String {
    carry.extend_from_slice(data);
    let valid = match std::str::from_utf8(carry) {
        Ok(s) => s.len(),
        Err(e) if e.error_len().is_some() => carry.len(),
        Err(e) => e.valid_up_to(),
    };
    let s = String::from_utf8_lossy(&carry[..valid]).into_owned();
    carry.drain(..valid);
    s
}

/// At most `max` bytes of `q` that end on a rune's end, taken off it.
fn take_runes(q: &mut Vec<u8>, max: usize) -> Vec<u8> {
    let mut n = q.len().min(max);
    while n > 0 && n < q.len() && (q[n] & 0xC0) == 0x80 {
        n -= 1;
    }
    q.drain(..n).collect()
}

/// A wctl request: its verb and its options and arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Wctl {
    pub verb: String,
    pub r: Option<Rect>,
    pub dx: Option<i32>,
    pub dy: Option<i32>,
    pub minx: Option<i32>,
    pub miny: Option<i32>,
    /// For `new`: the command, the rest of the line.
    pub cmd: String,
}

/// Parses a wctl request, as rio(4) writes them: a verb, then options
/// `-r x0 y0 x1 y1`, `-dx n`, `-dy n`, `-minx n`, `-miny n`, and for
/// `new` a command.
pub fn parse_wctl(s: &str) -> R<Wctl> {
    let mut t = s.split_whitespace().peekable();
    let mut w = Wctl {
        verb: t.next().ok_or(EBADWCTL)?.to_string(),
        ..Wctl::default()
    };
    let num = |v: Option<&str>| -> R<i32> {
        v.and_then(|v| v.parse().ok())
            .ok_or_else(|| String::from(EBADRECT))
    };
    while let Some(&o) = t.peek() {
        match o {
            "-r" => {
                t.next();
                let v = [
                    num(t.next())?,
                    num(t.next())?,
                    num(t.next())?,
                    num(t.next())?,
                ];
                w.r = Some(Rect::new(v[0], v[1], v[2], v[3]));
            }
            "-dx" => {
                t.next();
                w.dx = Some(num(t.next())?);
            }
            "-dy" => {
                t.next();
                w.dy = Some(num(t.next())?);
            }
            "-minx" => {
                t.next();
                w.minx = Some(num(t.next())?);
            }
            "-miny" => {
                t.next();
                w.miny = Some(num(t.next())?);
            }
            _ => break,
        }
    }
    let rest: Vec<&str> = t.collect();
    if !rest.is_empty() {
        if w.verb != "new" {
            return err(EBADWCTL);
        }
        w.cmd = rest.join(" ");
    }
    Ok(w)
}

impl Rio {
    /// Reads what window `sid`'s pipe holds and answers each whole message
    /// in it. False when the pipe has closed.
    pub fn input(&mut self, sid: u32) -> Result<bool, Error> {
        let Some(w) = self.win_mut(sid) else {
            return Ok(false);
        };
        let mut chunk = vec![0u8; MAX_MSIZE as usize];
        let n = w.sess.io.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return Ok(false);
        }
        w.sess.buf.extend_from_slice(&chunk[..n]);
        loop {
            let Some(w) = self.win_mut(sid) else {
                return Ok(true);
            };
            let Some(size) = purg_styx::message_size(&w.sess.buf) else {
                break;
            };
            if size < 7 || size > w.sess.msize as usize {
                return Ok(false);
            }
            if w.sess.buf.len() < size {
                break;
            }
            let m: Vec<u8> = w.sess.buf.drain(..size).collect();
            let msize = w.sess.msize;
            let Ok(msg) = Msg::decode(&m, msize) else {
                return Ok(false);
            };
            self.handle(sid, &msg)?;
        }
        Ok(true)
    }

    fn sess(&mut self, sid: u32) -> Option<&mut Session> {
        self.win_mut(sid).map(|w| &mut w.sess)
    }

    fn reply(&mut self, sid: u32, tag: u16, fcall: Fcall<'_>) -> Result<(), Error> {
        match self.sess(sid) {
            Some(s) => s.send(tag, fcall),
            None => Ok(()),
        }
    }

    fn handle(&mut self, sid: u32, msg: &Msg<'_>) -> Result<(), Error> {
        let tag = msg.tag;
        let r: R<Option<Vec<u8>>> = match &msg.fcall {
            Fcall::Tversion { msize, version } => {
                let Some(s) = self.sess(sid) else {
                    return Ok(());
                };
                s.msize = (*msize).clamp(IOHDRSZ + 1, MAX_MSIZE);
                s.fids.clear();
                s.reads.clear();
                let v = if version.starts_with("9P2000") {
                    "9P2000"
                } else {
                    "unknown"
                };
                let msize = s.msize;
                return s.send(tag, Fcall::Rversion { msize, version: v });
            }
            Fcall::Tauth { .. } => err("authentication not required"),
            Fcall::Tattach { fid, .. } => {
                let Some(s) = self.sess(sid) else {
                    return Ok(());
                };
                if s.fids.contains_key(fid) {
                    err("fid in use")
                } else {
                    s.fids.insert(*fid, Fid::new(Q::Root, sid));
                    return s.send(
                        tag,
                        Fcall::Rattach {
                            qid: qid(Q::Root, sid),
                        },
                    );
                }
            }
            Fcall::Tflush { oldtag } => {
                if let Some(s) = self.sess(sid) {
                    s.reads.retain(|p| p.tag != *oldtag);
                }
                return self.reply(sid, tag, Fcall::Rflush);
            }
            Fcall::Twalk {
                fid,
                newfid,
                wnames,
            } => match self.walk(sid, *fid, *newfid, wnames) {
                Ok(q) => return self.reply(sid, tag, Fcall::Rwalk { wqids: q }),
                Err(e) => Err(e),
            },
            Fcall::Topen { fid, mode } => match self.open(sid, *fid, *mode) {
                Ok(q) => {
                    let iounit = self.sess(sid).map_or(MAX_MSIZE, |s| s.msize) - IOHDRSZ;
                    return self.reply(sid, tag, Fcall::Ropen { qid: q, iounit });
                }
                Err(e) => Err(e),
            },
            Fcall::Tcreate { .. } | Fcall::Twstat { .. } => err(error::EPERM),
            Fcall::Tremove { fid } => {
                self.clunk(sid, *fid)?;
                err(error::EPERM)
            }
            Fcall::Tread { fid, offset, count } => {
                let max = self.sess(sid).map_or(MAX_MSIZE, |s| s.msize) - IOHDRSZ;
                self.read(sid, tag, *fid, *offset, (*count).min(max))
            }
            Fcall::Twrite { fid, data, .. } => match self.write(sid, *fid, data) {
                Ok(()) => {
                    return self.reply(
                        sid,
                        tag,
                        Fcall::Rwrite {
                            count: data.len() as u32,
                        },
                    );
                }
                Err(e) => Err(e),
            },
            Fcall::Tclunk { fid } => {
                self.clunk(sid, *fid)?;
                return self.reply(sid, tag, Fcall::Rclunk);
            }
            Fcall::Tstat { fid } => {
                let f = self
                    .sess(sid)
                    .and_then(|s| s.fids.get(fid))
                    .map(|f| (f.q, f.win));
                match f {
                    Some((q, win)) => {
                        let st = self.dir(q, win).pack().map_err(|e| Error(e.to_string()))?;
                        return self.reply(sid, tag, Fcall::Rstat { stat: &st });
                    }
                    None => err("unknown fid"),
                }
            }
            _ => err("bad 9P message"),
        };
        match r {
            Ok(Some(data)) => self.reply(sid, tag, Fcall::Rread { data: &data }),
            Ok(None) => Ok(()),
            Err(e) => self.reply(sid, tag, Fcall::Rerror { ename: &e }),
        }
    }

    fn dir(&self, q: Q, win: u32) -> Dir {
        let name = match q {
            Q::Root => String::from("/"),
            Q::Wsys => String::from("wsys"),
            Q::Dir => win.to_string(),
            _ => FILES
                .iter()
                .find(|e| e.1 == q)
                .map_or_else(String::new, |e| e.0.to_string()),
        };
        let mode = match q {
            Q::Root | Q::Wsys | Q::Dir => DMDIR | 0o500,
            _ => FILES.iter().find(|e| e.1 == q).map_or(0o600, |e| e.2),
        };
        Dir {
            qid: qid(q, win),
            mode,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            muid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn step(&self, q: Q, win: u32, name: &str) -> R<(Q, u32)> {
        if self.win(win).is_none() {
            return err(EDELETED);
        }
        match (q, name) {
            (_, "..") => Ok(match q {
                Q::Dir => (Q::Wsys, win),
                _ => (Q::Root, win),
            }),
            (Q::Root, "wsys") => Ok((Q::Wsys, win)),
            (Q::Wsys, n) => {
                let id: u32 = n.parse().map_err(|_| String::from(error::ENONEXIST))?;
                if self.win(id).is_none() || n != id.to_string() {
                    return err(error::ENONEXIST);
                }
                Ok((Q::Dir, id))
            }
            (Q::Root | Q::Dir, n) => FILES
                .iter()
                .find(|e| e.0 == n)
                .map(|e| (e.1, win))
                .ok_or_else(|| String::from(error::ENONEXIST)),
            _ => err(error::ENOTDIR),
        }
    }

    fn walk(&mut self, sid: u32, fid: u32, newfid: u32, names: &[&str]) -> R<Vec<Qid>> {
        let s = self.sess(sid).ok_or(EDELETED)?;
        let f = s.fids.get(&fid).ok_or("unknown fid")?;
        if f.open.is_some() {
            return err("walk of open fid");
        }
        if newfid != fid && s.fids.contains_key(&newfid) {
            return err("fid in use");
        }
        let (mut q, mut win) = (f.q, f.win);
        let mut qids = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match self.step(q, win, name) {
                Ok((nq, nw)) => {
                    q = nq;
                    win = nw;
                    qids.push(qid(q, win));
                }
                Err(e) if i == 0 => return Err(e),
                Err(_) => return Ok(qids),
            }
        }
        if let Some(s) = self.sess(sid) {
            s.fids.insert(newfid, Fid::new(q, win));
        }
        Ok(qids)
    }

    fn open(&mut self, sid: u32, fid: u32, omode: u8) -> R<Qid> {
        let s = self.sess(sid).ok_or(EDELETED)?;
        let f = s.fids.get(&fid).ok_or("unknown fid")?;
        if f.open.is_some() {
            return err("file already open");
        }
        let (q, win) = (f.q, f.win);
        let access = omode & mode::ACCESS;
        let write = access == mode::OWRITE || access == mode::ORDWR || omode & mode::OTRUNC != 0;
        let read = access != mode::OWRITE;
        if is_dir(q) && write {
            return err(error::EISDIR);
        }
        let p = FILES.iter().find(|e| e.1 == q).map_or(0o500, |e| e.2);
        if (write && p & 0o200 == 0) || (read && p & 0o400 == 0) {
            return err(error::EPERM);
        }
        let w = self.win_mut(win).ok_or(EDELETED)?;
        match q {
            Q::Pointer => {
                w.mouse_opens += 1;
                w.mouseq.clear();
            }
            Q::Keyboard => w.kbd_opens += 1,
            Q::Consctl => w.consctl_opens += 1,
            _ => {}
        }
        if let Some(f) = self.sess(sid).and_then(|s| s.fids.get_mut(&fid)) {
            f.open = Some(omode);
        }
        Ok(qid(q, win))
    }

    fn clunk(&mut self, sid: u32, fid: u32) -> Result<(), Error> {
        let Some(s) = self.sess(sid) else {
            return Ok(());
        };
        let Some(f) = s.fids.remove(&fid) else {
            return Ok(());
        };
        let gone: Vec<u16> = s
            .reads
            .iter()
            .filter(|p| p.fid == fid)
            .map(|p| p.tag)
            .collect();
        s.reads.retain(|p| p.fid != fid);
        for t in gone {
            s.send(
                t,
                Fcall::Rerror {
                    ename: error::EINTR,
                },
            )?;
        }
        if f.open.is_none() {
            return Ok(());
        }
        let mut redraw = false;
        if let Some(w) = self.win_mut(f.win) {
            match f.q {
                Q::Pointer => {
                    w.mouse_opens = w.mouse_opens.saturating_sub(1);
                    if w.mouse_opens == 0 {
                        // The program that drew in the window has
                        // finished with it: the text shows again.
                        w.mouseq.clear();
                        redraw = true;
                    }
                }
                Q::Keyboard => {
                    w.kbd_opens = w.kbd_opens.saturating_sub(1);
                    if w.kbd_opens == 0 {
                        // Keys nobody read are typing for the text.
                        let keys =
                            String::from_utf8_lossy(&std::mem::take(&mut w.kbdq)).into_owned();
                        for c in keys.chars() {
                            w.text.typed(c);
                        }
                        redraw = true;
                    }
                }
                Q::Consctl => {
                    w.consctl_opens = w.consctl_opens.saturating_sub(1);
                    if w.consctl_opens == 0 {
                        w.raw = false;
                    }
                }
                _ => {}
            }
        }
        if redraw {
            self.redraw(f.win);
        }
        Ok(())
    }

    fn read(&mut self, sid: u32, tag: u16, fid: u32, off: u64, count: u32) -> R<Option<Vec<u8>>> {
        let s = self.sess(sid).ok_or(EDELETED)?;
        let f = s.fids.get(&fid).ok_or("unknown fid")?;
        if f.open.is_none() {
            return err("file not open");
        }
        let (q, win) = (f.q, f.win);
        if self.win(win).is_none() {
            return err(EDELETED);
        }
        if matches!(q, Q::Cons | Q::Pointer | Q::Keyboard) {
            if let Some(data) = self.take(q, win, count as usize) {
                return Ok(Some(data));
            }
            if let Some(s) = self.sess(sid) {
                s.reads.push(Pending {
                    tag,
                    fid,
                    q,
                    win,
                    count,
                });
            }
            return Ok(None);
        }
        if off == 0 {
            let snap = match q {
                Q::Root | Q::Wsys | Q::Dir => {
                    let ents: Vec<(Q, u32)> = match q {
                        Q::Root => FILES
                            .iter()
                            .map(|e| (e.1, win))
                            .chain([(Q::Wsys, win)])
                            .collect(),
                        Q::Wsys => self.wins.iter().map(|w| (Q::Dir, w.id)).collect(),
                        _ => FILES.iter().map(|e| (e.1, win)).collect(),
                    };
                    let mut out = Vec::new();
                    for (eq, ew) in ents {
                        let _ = self.dir(eq, ew).pack_into(&mut out);
                    }
                    out
                }
                _ => self.contents(q, win).into_bytes(),
            };
            if let Some(f) = self.sess(sid).and_then(|s| s.fids.get_mut(&fid)) {
                f.snap = snap;
            }
        }
        let snap = &self
            .sess(sid)
            .and_then(|s| s.fids.get(&fid))
            .ok_or("unknown fid")?
            .snap;
        let start = (off as usize).min(snap.len());
        let mut end = (start + count as usize).min(snap.len());
        if is_dir(q) {
            // Whole entries only.
            let mut n = start;
            while n + 2 <= end {
                let len = u16::from_le_bytes([snap[n], snap[n + 1]]) as usize + 2;
                if n + len > end {
                    break;
                }
                n += len;
            }
            end = n;
        }
        Ok(Some(snap[start..end].to_vec()))
    }

    /// The text a plain file of window `win` reads as.
    fn contents(&self, q: Q, win: u32) -> String {
        let Some(w) = self.win(win) else {
            return String::new();
        };
        match q {
            Q::Label => w.label.clone(),
            Q::Text => w.text.all(),
            Q::Winid => format!("{:11} ", w.id),
            Q::Winname => w.name.clone(),
            Q::Wctl => format!(
                "{:11} {:11} {:11} {:11} {} {} ",
                w.r.min.x,
                w.r.min.y,
                w.r.max.x,
                w.r.max.y,
                if w.hidden { "hidden" } else { "visible" },
                if self.current == Some(win) {
                    "current"
                } else {
                    "notcurrent"
                }
            ),
            _ => String::new(),
        }
    }

    /// What a read of `cons`, `pointer` or `keyboard` of window `win`
    /// gets now, if anything.
    fn take(&mut self, q: Q, win: u32, count: usize) -> Option<Vec<u8>> {
        let w = self.win_mut(win)?;
        match q {
            Q::Pointer => w.mouseq.pop_front().map(String::into_bytes),
            Q::Keyboard if !w.kbdq.is_empty() => Some(take_runes(&mut w.kbdq, count)),
            Q::Cons if w.raw && !w.rawq.is_empty() => Some(take_runes(&mut w.rawq, count)),
            Q::Cons if !w.raw => w.text.read(count),
            _ => None,
        }
    }

    /// Answers the waiting reads that can be answered now.
    pub fn serve_reads(&mut self) {
        let sids: Vec<u32> = self.wins.iter().map(|w| w.id).collect();
        for sid in sids {
            let Some(s) = self.sess(sid) else {
                continue;
            };
            let reads = std::mem::take(&mut s.reads);
            let mut left = Vec::new();
            let mut answers = Vec::new();
            for p in reads {
                if self.win(p.win).is_none() {
                    answers.push((p.tag, Err(EDELETED)));
                    continue;
                }
                match self.take(p.q, p.win, p.count as usize) {
                    Some(d) => answers.push((p.tag, Ok(d))),
                    None => left.push(p),
                }
            }
            for (tag, a) in answers {
                let _ = match a {
                    Ok(d) => self.reply(sid, tag, Fcall::Rread { data: &d }),
                    Err(e) => self.reply(sid, tag, Fcall::Rerror { ename: e }),
                };
            }
            if let Some(s) = self.sess(sid) {
                left.append(&mut s.reads);
                s.reads = left;
            }
        }
    }

    fn write(&mut self, sid: u32, fid: u32, data: &[u8]) -> R<()> {
        let s = self.sess(sid).ok_or(EDELETED)?;
        let f = s.fids.get_mut(&fid).ok_or("unknown fid")?;
        if f.open.is_none() {
            return err("file not open");
        }
        let (q, win) = (f.q, f.win);
        let text = match q {
            Q::Cons => utf8(&mut f.carry, data),
            _ => String::from_utf8_lossy(data).into_owned(),
        };
        if self.win(win).is_none() {
            return err(EDELETED);
        }
        match q {
            Q::Cons => {
                if let Some(w) = self.win_mut(win) {
                    w.text.output(&text);
                    w.follow = true;
                }
                self.redraw(win);
                Ok(())
            }
            Q::Consctl => {
                let w = self.win_mut(win).ok_or(EDELETED)?;
                for c in text.split_whitespace() {
                    match c {
                        "rawon" => w.raw = true,
                        "rawoff" => w.raw = false,
                        "holdon" | "holdoff" => {}
                        _ => return err("unknown control message"),
                    }
                }
                Ok(())
            }
            Q::Label => {
                if let Some(w) = self.win_mut(win) {
                    w.label = text;
                }
                Ok(())
            }
            Q::Pointer => {
                let t = text.trim_start_matches('m');
                let v: Vec<i32> = t
                    .split_whitespace()
                    .filter_map(|v| v.parse().ok())
                    .collect();
                if v.len() < 2 {
                    return err("bad pointer message");
                }
                self.mouse.set(Point::new(v[0], v[1])).map_err(|e| e.0)?;
                Ok(())
            }
            Q::Wctl => {
                let w = parse_wctl(text.trim())?;
                self.wctl(win, w)
            }
            _ => err(error::EPERM),
        }
    }
}

impl Fid {
    fn new(q: Q, win: u32) -> Fid {
        Fid {
            q,
            win,
            open: None,
            snap: Vec::new(),
            carry: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wctl_requests() {
        let w = parse_wctl("new -r 10 20 300 400 echo hi").unwrap();
        assert_eq!(w.verb, "new");
        assert_eq!(w.r, Some(Rect::new(10, 20, 300, 400)));
        assert_eq!(w.cmd, "echo hi");
        let w = parse_wctl("move -minx 5 -miny 6").unwrap();
        assert_eq!((w.minx, w.miny), (Some(5), Some(6)));
        assert_eq!(parse_wctl("resize -r 1 2").unwrap_err(), EBADRECT);
        assert_eq!(parse_wctl("top now").unwrap_err(), EBADWCTL);
        assert_eq!(parse_wctl("").unwrap_err(), EBADWCTL);
    }

    #[test]
    fn whole_runes() {
        let mut q = "aé".as_bytes().to_vec();
        assert_eq!(take_runes(&mut q, 2), b"a");
        assert_eq!(take_runes(&mut q, 2), "é".as_bytes());
    }
}
