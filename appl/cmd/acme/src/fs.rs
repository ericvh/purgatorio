//! acme(4): the file system acme serves on `/mnt/acme`.
//!
//! acme serves Styx itself, in its one event loop. At start it forks its
//! namespace, makes a pipe and starts a copy of itself (`acme --mount`)
//! that shares the namespace and mounts the pipe's far end on
//! `/mnt/acme`; acme answers that mount's version and attach before it
//! draws anything. From then on the pipe is one more fd the loop waits
//! on with [`purgatorio::ready`]: when it is readable acme reads what
//! came, answers every whole message in it, and goes back to the mouse
//! and keyboard. Commands acme runs inherit the namespace, so they see
//! the files.
//!
//! Requests are answered at once, except a read of an `event` file with
//! no event queued: it is kept, with its tag, and answered when an event
//! comes ([`Fs::flush`], run after everything the loop does), or with an
//! error when its window goes. A `Tflush` forgets it, which is what
//! happens when the reading prog is killed; the clunk that follows
//! closes the file, and with the last `event` closed the window acts on
//! its own mouse actions again.
//!
//! The tree: `cons`, `consctl`, `index`, `label`, `new/` and a directory
//! `<id>/` per window with `addr`, `body`, `ctl`, `data`, `event`, `tag`
//! and `xdata`. `new` holds the same files; opening one of them makes a
//! window, and the open file is that window's. (Making it on the walk,
//! as Plan 9's acme does, would make two here: an open walks once to
//! stat the file and again to open it.)

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use purg_styx::{DMAPPEND, DMDIR, Dir, Fcall, IOHDRSZ, MAX_MSIZE, Msg, QTDIR, QTFILE, Qid};
use purgatorio::Error;
use purgatorio::abi::{error, mode, mount, pctl};

use crate::text::EVENTSIZE;
use crate::{Acme, Tid, addr};

pub const MTPT: &str = "/mnt/acme";

const EDEL: &str = "deleted window";
const EBADCTL: &str = "ill-formed control message";
const EBADEVENT: &str = "bad event syntax";

/// The files, numbered for qid paths: `(window id << 8) | file`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Q {
    Root = 0,
    Cons,
    Consctl,
    Index,
    Label,
    New,
    Dir,
    Addr,
    Body,
    Ctl,
    Data,
    Event,
    Tag,
    Xdata,
}

const TOP: &[(&str, Q, u32)] = &[
    ("cons", Q::Cons, 0o600),
    ("consctl", Q::Consctl, 0o400),
    ("index", Q::Index, 0o400),
    ("label", Q::Label, 0o600),
    ("new", Q::New, DMDIR | 0o500),
];

const WIN: &[(&str, Q, u32)] = &[
    ("addr", Q::Addr, 0o600),
    ("body", Q::Body, DMAPPEND | 0o600),
    ("ctl", Q::Ctl, 0o600),
    ("data", Q::Data, 0o600),
    ("event", Q::Event, 0o600),
    ("tag", Q::Tag, DMAPPEND | 0o600),
    ("xdata", Q::Xdata, 0o600),
];

fn is_dir(q: Q) -> bool {
    matches!(q, Q::Root | Q::New | Q::Dir)
}

fn perm(q: Q) -> u32 {
    match q {
        Q::Root | Q::Dir => DMDIR | 0o700,
        _ => TOP
            .iter()
            .chain(WIN)
            .find(|e| e.1 == q)
            .map_or(0o600, |e| e.2),
    }
}

fn qid(q: Q, win: u64) -> Qid {
    Qid {
        path: (win << 8) | q as u64,
        vers: 0,
        qtype: if is_dir(q) { QTDIR } else { QTFILE },
    }
}

struct Fid {
    q: Q,
    win: u64,
    open: Option<u8>,
    /// What a read at offset 0 found, for reads further on.
    snap: Vec<u8>,
    /// The bytes of a rune split between writes.
    carry: Vec<u8>,
}

/// A read of `event` waiting for an event.
struct Pending {
    tag: u16,
    fid: u32,
    win: u64,
    count: u32,
}

pub struct Fs {
    io: File,
    buf: Vec<u8>,
    msize: u32,
    fids: HashMap<u32, Fid>,
    reads: Vec<Pending>,
    user: String,
    label: String,
    attached: bool,
}

/// `acme --mount`: mounts fd 0 on `/mnt/acme` in the namespace it shares
/// with acme.
pub fn mount_helper() -> Result<(), Error> {
    purgatorio::mount(0, -1, MTPT, mount::MREPL | mount::MNOAUTH, "")
        .map_err(|e| Error(format!("acme: cannot mount on {MTPT}: {e}")))
}

/// Forks acme's namespace and mounts its file system there, answering
/// the mount's messages until it has attached. Returns `None`, after
/// saying why, when that cannot be done; acme works on without it.
pub fn start(a: &mut Acme) -> Option<Fs> {
    match try_start(a) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("acme: no {MTPT}: {e}");
            None
        }
    }
}

fn try_start(a: &mut Acme) -> Result<Option<Fs>, Error> {
    purgatorio::pctl(pctl::FORKNS, &[])?;
    if purgatorio::stat(MTPT).is_err() {
        // Two acmes may race to make it, so only the stat decides.
        let _ = std::fs::create_dir_all(MTPT);
        purgatorio::stat(MTPT)?;
    }
    let [ours, theirs] = purgatorio::pipe()?;
    purgatorio::spawn(
        "/bin/acme.wasm",
        &[String::from("acme"), String::from("--mount")],
        pctl::NEWFD,
        &[(theirs.as_raw_fd(), 0), (2, 2)],
    )?;
    drop(theirs);
    let user = std::fs::read_to_string("/dev/user").unwrap_or_else(|_| String::from("acme"));
    let mut fs = Fs {
        io: File::from(ours),
        buf: Vec::new(),
        msize: MAX_MSIZE,
        fids: HashMap::new(),
        reads: Vec::new(),
        user,
        label: String::new(),
        attached: false,
    };
    while !fs.attached {
        // The helper exiting without mounting ends the pipe.
        if !fs.input(a)? {
            return Ok(None);
        }
    }
    Ok(Some(fs))
}

/// One acme(4) event: origin, type, the range, the flag and the text,
/// which is left out (with a count of 0) when it is [`EVENTSIZE`] runes
/// or longer.
pub fn event(origin: char, kind: char, q0: usize, q1: usize, flag: u32, text: &str) -> String {
    let n = text.chars().count();
    if n < EVENTSIZE {
        format!("{origin}{kind}{q0} {q1} {flag} {n} {text}\n")
    } else {
        format!("{origin}{kind}{q0} {q1} {flag} 0 \n")
    }
}

/// Parses the events written to an `event` file: each is the origin and
/// type characters and the range, optionally followed by the flag, the
/// count and that many runes of text, and a newline.
pub fn parse_events(s: &str) -> Result<Vec<(char, char, usize, usize)>, &'static str> {
    let mut out = Vec::new();
    let mut cs = s.chars().peekable();
    loop {
        while cs.peek() == Some(&'\n') {
            cs.next();
        }
        let Some(origin) = cs.next() else {
            return Ok(out);
        };
        let kind = cs.next().ok_or(EBADEVENT)?;
        let mut nums = Vec::new();
        while nums.len() < 4 {
            while cs.peek() == Some(&' ') {
                cs.next();
            }
            if !cs.peek().is_some_and(char::is_ascii_digit) {
                break;
            }
            let mut n = String::new();
            while let Some(c) = cs.peek().copied().filter(char::is_ascii_digit) {
                n.push(c);
                cs.next();
            }
            nums.push(n.parse::<usize>().map_err(|_| EBADEVENT)?);
        }
        if nums.len() < 2 {
            return Err(EBADEVENT);
        }
        if nums.len() == 4 {
            // One blank, then the text: `count` runes, which may be
            // blanks or newlines.
            if cs.peek() == Some(&' ') {
                cs.next();
            }
            for _ in 0..nums[3] {
                cs.next().ok_or(EBADEVENT)?;
            }
        }
        match cs.next() {
            None | Some('\n') => {}
            Some(_) if nums.len() < 4 => {
                // Text without a count: the rest of the line.
                for c in cs.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            Some(_) => return Err(EBADEVENT),
        }
        out.push((origin, kind, nums[0], nums[1]));
    }
}

/// The text of a window's `ctl` file: id, tag and body lengths in runes,
/// whether it is a directory and modified, the body's width, the font
/// and the tab width, as acme(4) lists them.
fn ctl_text(a: &mut Acme, id: u64) -> Option<String> {
    let (font, tabs) = {
        let w = a.win(id)?;
        (a.font_name(&w.body), w.body.tab)
    };
    let tab = {
        let (_, f) = a.text_font(Tid::Body(id))?;
        f.use_tab(tabs);
        crate::frame::Widths::tab(f)
    };
    let w = a.win(id)?;
    Some(format!(
        "{:11} {:11} {:11} {:11} {:11} {:11} {} {:11} ",
        id,
        w.tag.b.len(),
        w.body.b.len(),
        u8::from(w.is_dir),
        u8::from(w.body.b.dirty()),
        w.body.r.dx(),
        font,
        tab
    ))
}

fn index_text(a: &Acme) -> String {
    let mut s = String::new();
    for w in a.cols.iter().flat_map(|c| c.wins.iter()) {
        let tag = w.tag.b.all();
        let tag = tag.split('\n').next().unwrap_or("");
        s.push_str(&format!(
            "{:11} {:11} {:11} {:11} {:11} {}\n",
            w.id,
            w.tag.b.len(),
            w.body.b.len(),
            u8::from(w.is_dir),
            u8::from(w.body.b.dirty()),
            tag
        ));
    }
    s
}

/// Up to `max` bytes of whole runes of `b` from rune `q0` to `q1`: the
/// string and how many runes it holds.
fn runes(b: &crate::text::Buffer, q0: usize, q1: usize, max: usize) -> (String, usize) {
    let mut s = String::new();
    let mut q = q0;
    while q < q1 {
        let c = b.at(q);
        if s.len() + c.len_utf8() > max {
            break;
        }
        s.push(c);
        q += 1;
    }
    (s, q - q0)
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

type R<T> = Result<T, String>;

fn err<T>(s: &str) -> R<T> {
    Err(String::from(s))
}

impl Fs {
    pub fn fd(&self) -> i32 {
        self.io.as_raw_fd()
    }

    /// Reads what the pipe holds and answers each whole message in it.
    /// False when the pipe has closed.
    pub fn input(&mut self, a: &mut Acme) -> Result<bool, Error> {
        let mut chunk = vec![0u8; MAX_MSIZE as usize];
        let n = self.io.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&chunk[..n]);
        while let Some(size) = purg_styx::message_size(&self.buf) {
            if size < 7 || size > self.msize as usize {
                return Err(Error::from("acme: bad 9P message"));
            }
            if self.buf.len() < size {
                break;
            }
            let m: Vec<u8> = self.buf.drain(..size).collect();
            let msg = Msg::decode(&m, self.msize).map_err(|e| Error(e.to_string()))?;
            self.handle(a, &msg)?;
        }
        Ok(true)
    }

    fn send(&mut self, tag: u16, fcall: Fcall<'_>) -> Result<(), Error> {
        let out = Msg { tag, fcall }
            .encode()
            .map_err(|e| Error(e.to_string()))?;
        self.io.write_all(&out)?;
        Ok(())
    }

    fn handle(&mut self, a: &mut Acme, msg: &Msg<'_>) -> Result<(), Error> {
        let tag = msg.tag;
        let r: R<Option<Vec<u8>>> = match &msg.fcall {
            Fcall::Tversion { msize, version } => {
                self.msize = (*msize).clamp(IOHDRSZ + 1, MAX_MSIZE);
                self.fids.clear();
                self.reads.clear();
                let v = if version.starts_with("9P2000") {
                    "9P2000"
                } else {
                    "unknown"
                };
                return self.send(
                    tag,
                    Fcall::Rversion {
                        msize: self.msize,
                        version: v,
                    },
                );
            }
            Fcall::Tauth { .. } => err("authentication not required"),
            Fcall::Tattach { fid, .. } => {
                if self.fids.contains_key(fid) {
                    err("fid in use")
                } else {
                    self.fids.insert(*fid, Fid::new(Q::Root, 0));
                    self.attached = true;
                    return self.send(
                        tag,
                        Fcall::Rattach {
                            qid: qid(Q::Root, 0),
                        },
                    );
                }
            }
            Fcall::Tflush { oldtag } => {
                self.reads.retain(|p| p.tag != *oldtag);
                return self.send(tag, Fcall::Rflush);
            }
            Fcall::Twalk {
                fid,
                newfid,
                wnames,
            } => match self.walk(a, *fid, *newfid, wnames) {
                Ok(q) => return self.send(tag, Fcall::Rwalk { wqids: q }),
                Err(e) => Err(e),
            },
            Fcall::Topen { fid, mode } => match self.open(a, *fid, *mode) {
                Ok(q) => {
                    let iounit = self.msize - IOHDRSZ;
                    return self.send(tag, Fcall::Ropen { qid: q, iounit });
                }
                Err(e) => Err(e),
            },
            Fcall::Tcreate { .. } | Fcall::Twstat { .. } => err(error::EPERM),
            Fcall::Tremove { fid } => {
                // A remove clunks the fid even when it fails.
                self.clunk(a, *fid)?;
                err(error::EPERM)
            }
            Fcall::Tread { fid, offset, count } => {
                let count = (*count).min(self.msize - IOHDRSZ);
                self.read(a, tag, *fid, *offset, count)
            }
            Fcall::Twrite { fid, data, .. } => match self.write(a, *fid, data) {
                Ok(()) => {
                    return self.send(
                        tag,
                        Fcall::Rwrite {
                            count: data.len() as u32,
                        },
                    );
                }
                Err(e) => Err(e),
            },
            Fcall::Tclunk { fid } => {
                self.clunk(a, *fid)?;
                return self.send(tag, Fcall::Rclunk);
            }
            Fcall::Tstat { fid } => match self.fids.get(fid) {
                Some(f) => {
                    let d = self.dir(a, f.q, f.win);
                    let st = d.pack().map_err(|e| Error(e.to_string()))?;
                    return self.send(tag, Fcall::Rstat { stat: &st });
                }
                None => err("unknown fid"),
            },
            _ => err("bad 9P message"),
        };
        match r {
            Ok(Some(data)) => self.send(tag, Fcall::Rread { data: &data }),
            // A read that waits for an event.
            Ok(None) => Ok(()),
            Err(e) => self.send(tag, Fcall::Rerror { ename: &e }),
        }
    }

    fn dir(&self, a: &Acme, q: Q, win: u64) -> Dir {
        let name = match q {
            Q::Root => String::from("/"),
            Q::Dir => win.to_string(),
            _ => TOP
                .iter()
                .chain(WIN)
                .find(|e| e.1 == q)
                .map_or_else(String::new, |e| e.0.to_string()),
        };
        let length = match (q, a.win(win)) {
            (Q::Body, Some(w)) => w.body.b.all().len() as u64,
            (Q::Tag, Some(w)) => w.tag.b.all().len() as u64,
            _ => 0,
        };
        Dir {
            qid: qid(q, win),
            mode: perm(q),
            length,
            name,
            uid: self.user.clone(),
            gid: self.user.clone(),
            muid: self.user.clone(),
            ..Dir::default()
        }
    }

    fn walk(&mut self, a: &mut Acme, fid: u32, newfid: u32, names: &[&str]) -> R<Vec<Qid>> {
        let f = self.fids.get(&fid).ok_or("unknown fid")?;
        if f.open.is_some() {
            return err("walk of open fid");
        }
        if newfid != fid && self.fids.contains_key(&newfid) {
            return err("fid in use");
        }
        let (mut q, mut win) = (f.q, f.win);
        let mut qids = Vec::new();
        for (i, name) in names.iter().enumerate() {
            match step(a, q, win, name) {
                Ok((nq, nw)) => {
                    q = nq;
                    win = nw;
                    qids.push(qid(q, win));
                }
                Err(e) if i == 0 => return Err(e),
                Err(_) => return Ok(qids),
            }
        }
        self.fids.insert(newfid, Fid::new(q, win));
        Ok(qids)
    }

    fn open(&mut self, a: &mut Acme, fid: u32, omode: u8) -> R<Qid> {
        let f = self.fids.get_mut(&fid).ok_or("unknown fid")?;
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
        let p = perm(q);
        if (write && p & 0o200 == 0) || (read && p & 0o400 == 0) {
            return err(error::EPERM);
        }
        let mut win = win;
        if win == 0 && !is_dir(q) && WIN.iter().any(|e| e.1 == q) {
            // A file under `new`: opening it makes the window.
            let ci = a.cols.len().saturating_sub(1);
            win = a.new_win(ci, "", "", false);
            f.win = win;
        }
        if win != 0 {
            let w = a.win_mut(win).ok_or(EDEL)?;
            match q {
                Q::Addr => {
                    if w.fs.naddr == 0 {
                        w.fs.addr = (0, 0);
                    }
                    w.fs.naddr += 1;
                }
                Q::Event => {
                    w.fs.nevent += 1;
                    w.body.b.set_watch(true);
                    w.tag.b.set_watch(true);
                }
                _ => {}
            }
        }
        f.open = Some(omode);
        Ok(qid(q, win))
    }

    fn clunk(&mut self, a: &mut Acme, fid: u32) -> Result<(), Error> {
        let Some(f) = self.fids.remove(&fid) else {
            return Ok(());
        };
        // Reads still waiting have been flushed if their prog went; any
        // left get an answer, so no tag is lost.
        let gone: Vec<u16> = self
            .reads
            .iter()
            .filter(|p| p.fid == fid)
            .map(|p| p.tag)
            .collect();
        self.reads.retain(|p| p.fid != fid);
        for t in gone {
            self.send(t, Fcall::Rerror { ename: EDEL })?;
        }
        if f.open.is_none() {
            return Ok(());
        }
        // Another view of the body may still read its events.
        let watched = a.win(f.win).is_some_and(|w| {
            let k = w.body.b.key();
            a.cols
                .iter()
                .flat_map(|c| c.wins.iter())
                .any(|o| o.id != w.id && o.body.b.key() == k && o.fs.nevent > 0)
        });
        if let Some(w) = a.win_mut(f.win) {
            match f.q {
                Q::Addr => w.fs.naddr = w.fs.naddr.saturating_sub(1),
                Q::Event => {
                    w.fs.nevent = w.fs.nevent.saturating_sub(1);
                    if w.fs.nevent == 0 {
                        w.fs.events.clear();
                        w.tag.b.set_watch(false);
                        w.tag.b.take_changes();
                        if !watched {
                            w.body.b.set_watch(false);
                            w.body.b.take_changes();
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read(
        &mut self,
        a: &mut Acme,
        tag: u16,
        fid: u32,
        off: u64,
        count: u32,
    ) -> R<Option<Vec<u8>>> {
        let f = self.fids.get(&fid).ok_or("unknown fid")?;
        if f.open.is_none() {
            return err("file not open");
        }
        let (q, win) = (f.q, f.win);
        if win != 0 && a.win(win).is_none() {
            return err(EDEL);
        }
        let max = count as usize;
        match q {
            Q::Event => {
                let w = a.win_mut(win).ok_or(EDEL)?;
                if w.fs.events.is_empty() {
                    self.reads.push(Pending {
                        tag,
                        fid,
                        win,
                        count,
                    });
                    return Ok(None);
                }
                let n = w.fs.events.len().min(max);
                return Ok(Some(w.fs.events.drain(..n).collect()));
            }
            Q::Data | Q::Xdata => {
                let w = a.win_mut(win).ok_or(EDEL)?;
                let (q0, q1) = w.fs.addr;
                let len = w.body.b.len();
                let end = if q == Q::Data { len } else { q1.min(len) };
                if q0 > len || (q == Q::Xdata && q0 > end) {
                    return err(addr::ERANGE);
                }
                let (s, n) = runes(&w.body.b, q0, end, max);
                w.fs.addr.0 = q0 + n;
                if q == Q::Data {
                    w.fs.addr.1 = w.fs.addr.0;
                }
                return Ok(Some(s.into_bytes()));
            }
            _ => {}
        }
        if off == 0 {
            let snap = match q {
                Q::Root | Q::Dir => {
                    let mut out = Vec::new();
                    let ents: Vec<(Q, u64)> = if q == Q::Root {
                        TOP.iter()
                            .map(|e| (e.1, 0))
                            .chain(a.ids().into_iter().map(|id| (Q::Dir, id)))
                            .collect()
                    } else {
                        WIN.iter().map(|e| (e.1, win)).collect()
                    };
                    for (eq, ew) in ents {
                        let _ = self.dir(a, eq, ew).pack_into(&mut out);
                    }
                    out
                }
                Q::Index => index_text(a).into_bytes(),
                Q::Label => self.label.clone().into_bytes(),
                Q::Body => a
                    .win(win)
                    .map(|w| w.body.b.all())
                    .unwrap_or_default()
                    .into_bytes(),
                Q::Tag => a
                    .win(win)
                    .map(|w| w.tag.b.all())
                    .unwrap_or_default()
                    .into_bytes(),
                Q::Ctl => ctl_text(a, win).unwrap_or_default().into_bytes(),
                Q::Addr => a
                    .win(win)
                    .map(|w| format!("{:11} {:11} ", w.fs.addr.0, w.fs.addr.1))
                    .unwrap_or_default()
                    .into_bytes(),
                _ => Vec::new(),
            };
            if let Some(f) = self.fids.get_mut(&fid) {
                f.snap = snap;
            }
        }
        let snap = &self.fids.get(&fid).ok_or("unknown fid")?.snap;
        let start = (off as usize).min(snap.len());
        let mut end = (start + max).min(snap.len());
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

    fn write(&mut self, a: &mut Acme, fid: u32, data: &[u8]) -> R<()> {
        let f = self.fids.get_mut(&fid).ok_or("unknown fid")?;
        match f.open {
            Some(m) if m & mode::ACCESS != mode::OREAD => {}
            _ => return err("file not open for writing"),
        }
        let (q, win) = (f.q, f.win);
        let s = utf8(&mut f.carry, data);
        if win != 0 && a.win(win).is_none() {
            return err(EDEL);
        }
        match q {
            Q::Cons => {
                let cwd = a.cwd.clone();
                a.append(&cwd, &s);
                a.post_changes('F');
            }
            Q::Label => self.label = s,
            Q::Body | Q::Tag => {
                let t = if q == Q::Body {
                    Tid::Body(win)
                } else {
                    Tid::Tag(win)
                };
                let nomark = a.win(win).is_some_and(|w| w.fs.nomark);
                if let Some(x) = a.text_mut(t) {
                    if !nomark {
                        x.b.mark();
                    }
                    let n = x.b.len();
                    let follow = x.edit(n, n, &s);
                    let cursor = x.q0;
                    if follow && q == Q::Body {
                        a.show(t, cursor);
                    }
                }
                a.edited(t).map_err(|e| e.0)?;
                a.post_changes('E');
            }
            Q::Data | Q::Xdata => {
                let w = a.win_mut(win).ok_or(EDEL)?;
                let (q0, q1) = w.fs.addr;
                let len = w.body.b.len();
                if q0 > len || q1 > len {
                    return err(addr::ERANGE);
                }
                if !w.fs.nomark {
                    w.body.b.mark();
                }
                let follow = w.body.edit(q0, q1, &s);
                let end = q0 + s.chars().count();
                w.fs.addr = (end, end);
                let cursor = w.body.q0;
                if follow {
                    a.show(Tid::Body(win), cursor);
                }
                a.edited(Tid::Body(win)).map_err(|e| e.0)?;
                a.post_changes('F');
            }
            Q::Addr => {
                let w = a.win_mut(win).ok_or(EDEL)?;
                let text: Vec<char> = w.body.b.all().chars().collect();
                w.fs.addr = addr::eval(&text, w.fs.addr, &s)?;
            }
            Q::Ctl => {
                for line in s.lines() {
                    ctl(a, win, line)?;
                }
                a.post_changes('F');
            }
            Q::Event => {
                for (_, kind, q0, q1) in parse_events(&s)? {
                    let t = match kind {
                        'X' | 'L' => Tid::Body(win),
                        'x' | 'l' => Tid::Tag(win),
                        _ => return err(EBADEVENT),
                    };
                    let n = a.text(t).map_or(0, |x| x.b.len());
                    if q0 > q1 || q1 > n {
                        return err(EBADEVENT);
                    }
                    if kind.eq_ignore_ascii_case(&'x') {
                        let s = a.expand(t, q0, q1, false);
                        a.execute(t, &s).map_err(|e| e.0)?;
                    } else {
                        let s = a.expand(t, q0, q1, true);
                        a.look(t, &s).map_err(|e| e.0)?;
                    }
                    a.post_changes('M');
                }
            }
            _ => return err(error::EPERM),
        }
        Ok(())
    }

    /// Answers the reads of `event` that can be: with the events that
    /// came, or with an error once their window has gone.
    pub fn flush(&mut self, a: &mut Acme) -> Result<(), Error> {
        let mut i = 0;
        while i < self.reads.len() {
            let p = &self.reads[i];
            let (tag, win, max) = (p.tag, p.win, p.count as usize);
            match a.win_mut(win) {
                None => {
                    self.reads.remove(i);
                    self.send(tag, Fcall::Rerror { ename: EDEL })?;
                }
                Some(w) if !w.fs.events.is_empty() => {
                    self.reads.remove(i);
                    let n = w.fs.events.len().min(max);
                    let data: Vec<u8> = w.fs.events.drain(..n).collect();
                    self.send(tag, Fcall::Rread { data: &data })?;
                }
                Some(_) => i += 1,
            }
        }
        Ok(())
    }
}

impl Fid {
    fn new(q: Q, win: u64) -> Fid {
        Fid {
            q,
            win,
            open: None,
            snap: Vec::new(),
            carry: Vec::new(),
        }
    }
}

/// One step of a walk from `(q, win)`.
fn step(a: &mut Acme, q: Q, win: u64, name: &str) -> R<(Q, u64)> {
    if !is_dir(q) {
        return err(error::ENOTDIR);
    }
    if name == ".." {
        return Ok((Q::Root, 0));
    }
    if q == Q::Root {
        if let Some(e) = TOP.iter().find(|e| e.0 == name) {
            return Ok((e.1, 0));
        }
        if let Ok(id) = name.parse::<u64>()
            && a.win(id).is_some()
        {
            return Ok((Q::Dir, id));
        }
        return err(error::ENONEXIST);
    }
    // Under `new` the window is made when a file is opened.
    if q == Q::Dir && a.win(win).is_none() {
        return err(EDEL);
    }
    match WIN.iter().find(|e| e.0 == name) {
        Some(e) => Ok((e.1, win)),
        None => err(error::ENONEXIST),
    }
}

/// One message written to a window's `ctl`.
fn ctl(a: &mut Acme, id: u64, line: &str) -> R<()> {
    let line = line.trim_end();
    if line.is_empty() {
        return Ok(());
    }
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, r)) => (c, r.trim()),
        None => (line, ""),
    };
    match cmd {
        "name" => {
            if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return err("bad character in file name");
            }
            let w = a.win_mut(id).ok_or(EDEL)?;
            crate::set_tag(w, arg);
        }
        "dump" => a.win_mut(id).ok_or(EDEL)?.fs.dump = Some(arg.to_string()),
        "dumpdir" => a.win_mut(id).ok_or(EDEL)?.fs.dumpdir = Some(arg.to_string()),
        "clean" | "dirty" => {
            let w = a.win_mut(id).ok_or(EDEL)?;
            if cmd == "clean" {
                w.body.b.set_clean();
            } else {
                w.body.b.set_dirty();
            }
            w.warned = false;
            a.retag(id);
        }
        "show" => {
            let q = a.win(id).ok_or(EDEL)?.body.q0;
            a.show(Tid::Body(id), q);
        }
        "del" => {
            let w = a.win(id).ok_or(EDEL)?;
            if w.modified() && !w.warned {
                let (n, dir) = (w.name(), w.dir(&a.cwd));
                a.error(&dir, &format!("{n} modified"));
                if let Some(w) = a.win_mut(id) {
                    w.warned = true;
                }
                return err("file dirty");
            }
            a.del_win(id);
            return Ok(());
        }
        "delete" => {
            a.del_win(id);
            return Ok(());
        }
        "get" => a.get(id, ""),
        "put" => a.put(id, ""),
        "addr=dot" => {
            let w = a.win_mut(id).ok_or(EDEL)?;
            w.fs.addr = (w.body.q0, w.body.q1);
        }
        "dot=addr" => {
            let w = a.win_mut(id).ok_or(EDEL)?;
            let (q0, q1) = w.fs.addr;
            w.body.set_select(q0, q1);
        }
        "mark" => a.win_mut(id).ok_or(EDEL)?.body.b.mark(),
        "nomark" => a.win_mut(id).ok_or(EDEL)?.fs.nomark = true,
        "cleartag" => {
            let w = a.win_mut(id).ok_or(EDEL)?;
            let t = w.tag.b.all();
            if let Some(i) = t.find('|') {
                let bar = t[..i].chars().count() + 1;
                let n = w.tag.b.len();
                w.tag.b.delete(bar, n);
                w.tag.set_select(bar, bar);
            }
        }
        _ => return err(EBADCTL),
    }
    a.touch(Tid::Tag(id));
    a.touch(Tid::Body(id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events() {
        assert_eq!(event('M', 'X', 4, 7, 1, "Del"), "MX4 7 1 3 Del\n");
        assert_eq!(event('K', 'I', 0, 1, 0, "\n"), "KI0 1 0 1 \n\n");
        assert_eq!(event('E', 'D', 3, 9, 0, ""), "ED3 9 0 0 \n");
        let long = "x".repeat(EVENTSIZE);
        assert_eq!(event('M', 'x', 0, 256, 0, &long), "Mx0 256 0 0 \n");
    }

    #[test]
    fn parsing() {
        assert_eq!(parse_events("MX4 7 1 3 Del\n"), Ok(vec![('M', 'X', 4, 7)]));
        assert_eq!(
            parse_events("MX4 4 3 0 \nMX4 7 0 3 Del\n"),
            Ok(vec![('M', 'X', 4, 4), ('M', 'X', 4, 7)])
        );
        // Text holding a newline, counted in runes.
        assert_eq!(
            parse_events("ML0 3 0 3 a\nb\nKl1 2\n"),
            Ok(vec![('M', 'L', 0, 3), ('K', 'l', 1, 2)])
        );
        assert_eq!(parse_events("Mx1 2 whatever\n"), Ok(vec![('M', 'x', 1, 2)]));
        assert_eq!(parse_events("KI0 2 0 2  x\n"), Ok(vec![('K', 'I', 0, 2)]));
        assert_eq!(parse_events("MX4\n"), Err(EBADEVENT));
        assert_eq!(parse_events("MX4 7 0 9 ab\n"), Err(EBADEVENT));
        assert_eq!(parse_events(""), Ok(vec![]));
    }

    #[test]
    fn qids() {
        assert_eq!(qid(Q::Dir, 3).path, 0x306);
        assert_eq!(qid(Q::Dir, 3).qtype, QTDIR);
        assert_eq!(perm(Q::Body), DMAPPEND | 0o600);
        assert_eq!(perm(Q::Consctl) & 0o200, 0);
    }
}
