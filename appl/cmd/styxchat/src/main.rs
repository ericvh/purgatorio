//! styxchat [-nsv] [-m messagesize] [dest]: hold a 9P conversation by
//! hand, for testing servers (and, with `-s`, clients).
//!
//! Each input line is one message, its words split with sh quoting;
//! blank lines and lines starting with `#` are skipped. As a client,
//! styxchat reads T-messages and assigns tags itself, from 1 up (`Tversion`
//! always uses NOTAG):
//!
//! ```text
//! Tversion messagesize version     Tauth afid uname aname
//! Tflush oldtag                    Tattach fid afid uname aname
//! Twalk fid newfid [name...]       Topen fid mode
//! Tcreate fid name perm mode       Tread fid offset count
//! Twrite fid offset data           Tclunk fid
//! Tremove fid                      Tstat fid
//! Twstat fid name uid gid mode mtime length
//! nexttag [tag]                    dump
//! ```
//!
//! `nofid` is NOFID and `~0` is all ones (in `Twstat`, "do not change");
//! numbers may be written with a radix, as `16rff`. With `-s` it is a
//! server: it prints the T-messages that arrive and reads R-messages, each
//! naming its tag (`notag` for NOTAG):
//!
//! ```text
//! Rversion tag messagesize version Rauth tag aqid
//! Rflush tag                       Rerror tag ename
//! Rattach tag qid                  Rwalk tag [qid ...]
//! Ropen tag qid iounit             Rcreate tag qid iounit
//! Rread tag data                   Rwrite tag count
//! Rclunk tag                       Rremove tag
//! Rstat tag qid mode atime mtime length name uid gid muid
//! Rwstat tag
//! ```
//!
//! A qid is `path.vers` with an optional `.type`, a number or the letters
//! `d` (directory), `a` (append only), `l` (exclusive) and `u` (auth).
//! Messages sent are printed after `-> `, messages received after `<- `,
//! in the text form of Inferno's Styx module; `-v` (or `dump`) adds a line
//! with the data of reads and writes. A bad line gets a `?` diagnostic.
//!
//! `dest` is a file opened for reading and writing; with `-n`, a network
//! address that is dialled (or, with `-s`, announced and listened on); or
//! a `{command}` run by sh with the conversation as its standard input and
//! output. Without `dest`, the conversation is standard input and the
//! messages are read from `/dev/cons`.
//!
//! So that a script reads in order, styxchat waits up to ten seconds for
//! the reply to each message (with `-s`, for the T-message an R-message
//! answers) before reading the next line, and at the end of its input
//! for the replies still due. A message that gets no reply in that time,
//! such as a read that blocks, can then be flushed by the next line.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use purg_styx::{Dir, Fcall, MAXWELEM, Msg, NOFID, NOTAG, Qid};
use purgatorio::abi::{mode, pctl};
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};
use styxserv::text;

const USAGE: &str = "styxchat [-nsv] [-m messagesize] [dest]";

/// How long to wait for a message before going on without it.
const PATIENCE: Duration = Duration::from_secs(10);

/// Events the helper progs write to the parent's event pipe.
const EV_TICK: u8 = b'T';
const EV_MSG: u8 = b'A';
const EV_HUP: u8 = b'H';
/// The event pipe's fd in the helpers.
const EVFD: i32 = 5;

/// A number as Limbo's `atoi`/`atob` read it, plus radix literals
/// (`16rff`) and `~0` for all ones.
fn num(s: &str) -> Option<u64> {
    if s == "~0" {
        return Some(u64::MAX);
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let v = match s.split_once('r') {
        Some((radix, digits)) => {
            let r: u32 = radix.parse().ok()?;
            if !(2..=36).contains(&r) {
                return None;
            }
            u64::from_str_radix(digits, r).ok()?
        }
        None => s.parse().ok()?,
    };
    Some(if neg { v.wrapping_neg() } else { v })
}

fn n32(s: &str) -> Result<u32, ()> {
    num(s).map(|v| v as u32).ok_or(())
}

fn fid(s: &str) -> Result<u32, ()> {
    match s {
        "nofid" | "NOFID" => Ok(NOFID),
        _ => n32(s),
    }
}

fn tag(s: &str) -> Result<u16, ()> {
    match s {
        "notag" | "NOTAG" | "~0" => Ok(NOTAG),
        _ => num(s).map(|v| v as u16).ok_or(()),
    }
}

fn qid(s: &str) -> Result<Qid, ()> {
    let mut parts = s.split('.');
    let path = num(parts.next().ok_or(())?).ok_or(())?;
    let vers = match parts.next() {
        Some(v) => v.parse().map_err(|_| ())?,
        None => 0,
    };
    let mut qtype = 0u8;
    if let Some(t) = parts.next() {
        if let Some(n) = num(t) {
            qtype = n as u8;
        } else {
            for c in t.chars() {
                match c {
                    'd' => qtype |= 0x80,
                    'a' => qtype |= 0x40,
                    'l' => qtype |= 0x20,
                    'u' => qtype |= 0x08,
                    'f' => {}
                    c => eprintln!("styxchat: unknown mode character {c}, ignoring"),
                }
            }
        }
    }
    if parts.next().is_some() {
        return Err(());
    }
    Ok(Qid { path, vers, qtype })
}

/// A parsed line: the message to send, or a command for styxchat itself.
enum Line {
    Send(Vec<u8>, Msg<'static>),
    NextTag(Option<u16>),
    Dump,
}

/// Leaks the words of a line so a `Msg` can borrow them for as long as
/// the line is in use; lines are short and few.
fn leak(v: Vec<String>) -> Vec<&'static str> {
    v.into_iter()
        .map(|s| &*Box::leak(s.into_boxed_str()))
        .collect()
}

fn usage(u: &str) -> String {
    format!("usage: {u}")
}

/// Parses a T-message line, given the tag it would get.
fn tmsg(w: &[&'static str], t: u16) -> Result<Line, String> {
    let n = w.len() - 1;
    let a = &w[1..];
    macro_rules! need {
        ($ok:expr, $u:expr) => {
            if !$ok {
                return Err(usage($u));
            }
        };
    }
    macro_rules! p {
        ($e:expr, $u:expr) => {
            $e.map_err(|_| usage($u))?
        };
    }
    let (tag, fcall) = match w[0] {
        "nexttag" => {
            return match a {
                [] => Ok(Line::NextTag(None)),
                [x] => Ok(Line::NextTag(Some(p!(tag(x), "nexttag [tag]")))),
                _ => Err(usage("nexttag [tag]")),
            };
        }
        "dump" => return Ok(Line::Dump),
        "Tversion" => {
            let u = "Tversion messagesize version";
            need!(n == 2, u);
            (
                NOTAG,
                Fcall::Tversion {
                    msize: p!(n32(a[0]), u),
                    version: a[1],
                },
            )
        }
        "Tauth" => {
            let u = "Tauth afid uname aname";
            need!(n == 3, u);
            (
                t,
                Fcall::Tauth {
                    afid: p!(fid(a[0]), u),
                    uname: a[1],
                    aname: a[2],
                },
            )
        }
        "Tflush" => {
            let u = "Tflush oldtag";
            need!(n == 1, u);
            (
                t,
                Fcall::Tflush {
                    oldtag: p!(tag(a[0]), u),
                },
            )
        }
        "Tattach" => {
            let u = "Tattach fid afid uname aname";
            need!(n == 4, u);
            (
                t,
                Fcall::Tattach {
                    fid: p!(fid(a[0]), u),
                    afid: p!(fid(a[1]), u),
                    uname: a[2],
                    aname: a[3],
                },
            )
        }
        "Twalk" => {
            let u = "Twalk fid newfid [name...]";
            need!(n >= 2 && n - 2 <= MAXWELEM, u);
            (
                t,
                Fcall::Twalk {
                    fid: p!(fid(a[0]), u),
                    newfid: p!(fid(a[1]), u),
                    wnames: a[2..].to_vec(),
                },
            )
        }
        "Topen" => {
            let u = "Topen fid mode";
            need!(n == 2, u);
            (
                t,
                Fcall::Topen {
                    fid: p!(fid(a[0]), u),
                    mode: p!(n32(a[1]), u) as u8,
                },
            )
        }
        "Tcreate" => {
            let u = "Tcreate fid name perm mode";
            need!(n == 4, u);
            (
                t,
                Fcall::Tcreate {
                    fid: p!(fid(a[0]), u),
                    name: a[1],
                    perm: p!(n32(a[2]), u),
                    mode: p!(n32(a[3]), u) as u8,
                },
            )
        }
        "Tread" => {
            let u = "Tread fid offset count";
            need!(n == 3, u);
            (
                t,
                Fcall::Tread {
                    fid: p!(fid(a[0]), u),
                    offset: p!(num(a[1]).ok_or(()), u),
                    count: p!(n32(a[2]), u),
                },
            )
        }
        "Twrite" => {
            let u = "Twrite fid offset data";
            need!(n == 3, u);
            (
                t,
                Fcall::Twrite {
                    fid: p!(fid(a[0]), u),
                    offset: p!(num(a[1]).ok_or(()), u),
                    data: a[2].as_bytes(),
                },
            )
        }
        "Tclunk" | "Tremove" | "Tstat" => {
            let u: &'static str = match w[0] {
                "Tclunk" => "Tclunk fid",
                "Tremove" => "Tremove fid",
                _ => "Tstat fid",
            };
            need!(n == 1, u);
            let f = p!(fid(a[0]), u);
            (
                t,
                match w[0] {
                    "Tclunk" => Fcall::Tclunk { fid: f },
                    "Tremove" => Fcall::Tremove { fid: f },
                    _ => Fcall::Tstat { fid: f },
                },
            )
        }
        "Twstat" => {
            let u = "Twstat fid name uid gid mode mtime length";
            need!(n == 7, u);
            let d = Dir {
                name: String::from(a[1]),
                uid: String::from(a[2]),
                gid: String::from(a[3]),
                mode: p!(n32(a[4]), u),
                mtime: p!(n32(a[5]), u),
                length: p!(num(a[6]).ok_or(()), u),
                ..Dir::null()
            };
            let stat: &'static [u8] = Box::leak(d.pack().map_err(|e| e.to_string())?.into());
            (
                t,
                Fcall::Twstat {
                    fid: p!(fid(a[0]), u),
                    stat,
                },
            )
        }
        _ => return Err(String::from("unknown message type")),
    };
    let m = Msg { tag, fcall };
    let b = m.encode().map_err(|e| e.to_string())?;
    Ok(Line::Send(b, m))
}

/// Parses an R-message line.
fn rmsg(w: &[&'static str]) -> Result<Line, String> {
    let n = w.len() - 1;
    let a = &w[1..];
    macro_rules! need {
        ($ok:expr, $u:expr) => {
            if !$ok {
                return Err(usage($u));
            }
        };
    }
    macro_rules! p {
        ($e:expr, $u:expr) => {
            $e.map_err(|_| usage($u))?
        };
    }
    let u: &'static str = match w[0] {
        "Rversion" => "Rversion tag messagesize version",
        "Rauth" => "Rauth tag aqid",
        "Rflush" => "Rflush tag",
        "Rerror" => "Rerror tag ename",
        "Rattach" => "Rattach tag qid",
        "Rwalk" => "Rwalk tag [qid ...]",
        "Ropen" => "Ropen tag qid iounit",
        "Rcreate" => "Rcreate tag qid iounit",
        "Rread" => "Rread tag data",
        "Rwrite" => "Rwrite tag count",
        "Rclunk" => "Rclunk tag",
        "Rremove" => "Rremove tag",
        "Rstat" => "Rstat tag qid mode atime mtime length name uid gid muid",
        "Rwstat" => "Rwstat tag",
        "dump" => return Ok(Line::Dump),
        _ => return Err(String::from("unknown message type")),
    };
    need!(n >= 1, u);
    let t = p!(tag(a[0]), u);
    let a = &a[1..];
    let n = n - 1;
    let fcall = match w[0] {
        "Rversion" => {
            need!(n == 2, u);
            Fcall::Rversion {
                msize: p!(n32(a[0]), u),
                version: a[1],
            }
        }
        "Rauth" => {
            need!(n == 1, u);
            Fcall::Rauth {
                aqid: p!(qid(a[0]), u),
            }
        }
        "Rattach" => {
            need!(n == 1, u);
            Fcall::Rattach {
                qid: p!(qid(a[0]), u),
            }
        }
        "Rerror" => {
            need!(n == 1, u);
            Fcall::Rerror { ename: a[0] }
        }
        "Rwalk" => {
            need!(n <= MAXWELEM, u);
            let mut wqids = Vec::new();
            for q in a {
                wqids.push(p!(qid(q), u));
            }
            Fcall::Rwalk { wqids }
        }
        "Ropen" | "Rcreate" => {
            need!(n == 2, u);
            let (q, iounit) = (p!(qid(a[0]), u), p!(n32(a[1]), u));
            if w[0] == "Ropen" {
                Fcall::Ropen { qid: q, iounit }
            } else {
                Fcall::Rcreate { qid: q, iounit }
            }
        }
        "Rread" => {
            need!(n == 1, u);
            Fcall::Rread {
                data: a[0].as_bytes(),
            }
        }
        "Rwrite" => {
            need!(n == 1, u);
            Fcall::Rwrite {
                count: p!(n32(a[0]), u),
            }
        }
        "Rstat" => {
            need!(n == 9, u);
            let q = p!(qid(a[0]), u);
            let d = Dir {
                qid: q,
                mode: p!(n32(a[1]), u) | (u32::from(q.qtype) << 24),
                atime: p!(n32(a[2]), u),
                mtime: p!(n32(a[3]), u),
                length: p!(num(a[4]).ok_or(()), u),
                name: String::from(a[5]),
                uid: String::from(a[6]),
                gid: String::from(a[7]),
                muid: String::from(a[8]),
                ..Dir::default()
            };
            let stat: &'static [u8] = Box::leak(d.pack().map_err(|e| e.to_string())?.into());
            Fcall::Rstat { stat }
        }
        _ => {
            need!(n == 0, u);
            match w[0] {
                "Rflush" => Fcall::Rflush,
                "Rclunk" => Fcall::Rclunk,
                "Rremove" => Fcall::Rremove,
                _ => Fcall::Rwstat,
            }
        }
    };
    let m = Msg { tag: t, fcall };
    let b = m.encode().map_err(|e| e.to_string())?;
    Ok(Line::Send(b, m))
}

fn show(dir: &str, m: &Msg<'_>, verbose: bool) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{dir} {}", text::msg(m));
    if verbose && let Some(d) = text::payload(m) {
        let _ = writeln!(out, "\t{}", text::data(d));
    }
}

/// The helper that reads the conversation: it prints each message that
/// arrives and reports its tag (and, for a flush reply, nothing more) on
/// the event pipe, fd [`EVFD`]. fd 0 is the conversation.
fn reader(verbose: bool, server: bool) -> Result<(), Error> {
    // SAFETY: the parent gave this prog these fds.
    let (mut conn, mut ev) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(EVFD)) };
    let peer = if server { "clients" } else { "server" };
    loop {
        let m = match styxserv::read_msg(&mut conn) {
            Ok(Some(m)) => m,
            // Killed at the end of the conversation.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Ok(None) | Err(_) => {
                eprintln!("styxchat: {peer} hungup");
                let _ = ev.write_all(&[EV_HUP]);
                return Ok(());
            }
        };
        match styxserv::decode(&m) {
            Ok(msg) => {
                show("<-", &msg, verbose);
                let t = msg.tag.to_le_bytes();
                ev.write_all(&[EV_MSG, t[0], t[1]])?;
            }
            Err(_) => eprintln!(
                "styxchat: {} conversion failed",
                if server { "T-message" } else { "R-message" }
            ),
        }
    }
}

/// The helper that wakes the parent while it waits, ten times a second.
fn ticker() -> Result<(), Error> {
    // SAFETY: fd [`EVFD`] is the parent's event pipe.
    let mut ev = unsafe { File::from_raw_fd(EVFD) };
    loop {
        std::thread::sleep(Duration::from_millis(100));
        ev.write_all(&[EV_TICK])?;
    }
}

/// What the parent knows from the event pipe.
struct Events {
    pipe: File,
    buf: Vec<u8>,
    /// Tags of messages that arrived and have not been accounted for.
    arrived: Vec<u16>,
    hungup: bool,
}

impl Events {
    /// Reads events until `done` says the wait is over, the peer hangs up
    /// or no message has arrived for [`PATIENCE`].
    fn wait(&mut self, mut done: impl FnMut(&mut Vec<u16>) -> bool) {
        let mut deadline = Instant::now() + PATIENCE;
        let mut chunk = [0u8; 256];
        loop {
            if done(&mut self.arrived) || self.hungup || Instant::now() >= deadline {
                return;
            }
            let n = match self.pipe.read(&mut chunk) {
                Ok(0) | Err(_) => {
                    self.hungup = true;
                    return;
                }
                Ok(n) => n,
            };
            self.buf.extend_from_slice(&chunk[..n]);
            let mut i = 0;
            while i < self.buf.len() {
                match self.buf[i] {
                    EV_MSG if i + 3 <= self.buf.len() => {
                        self.arrived
                            .push(u16::from_le_bytes([self.buf[i + 1], self.buf[i + 2]]));
                        deadline = Instant::now() + PATIENCE;
                        i += 3;
                    }
                    EV_MSG => break,
                    EV_HUP => {
                        self.hungup = true;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
            self.buf.drain(..i);
        }
    }
}

fn kill(pid: u32) {
    if let Ok(mut ctl) = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/prog/{pid}/ctl"))
    {
        let _ = ctl.write_all(b"kill");
    }
}

/// The conversation's fd, and what keeps it open.
fn connect(
    dest: Option<&str>,
    net: bool,
    server: bool,
) -> Result<(i32, Vec<OwnedFd>, Option<dial::Connection>), Error> {
    let Some(dest) = dest else {
        return Ok((0, Vec::new(), None));
    };
    if net {
        // A bare host gets the styx port, as Inferno's netmkaddr does.
        let addr = if dest.contains('!') {
            String::from(dest)
        } else {
            format!("tcp!{dest}!6666")
        };
        let conn = if server {
            let ann =
                dial::announce(&addr).map_err(|e| Error(format!("can't announce {addr}: {e}")))?;
            dial::listen(&ann.dir).map_err(|e| Error(format!("can't listen on {addr}: {e}")))?
        } else {
            dial::dial(&addr).map_err(|e| Error(format!("can't dial {addr}: {e}")))?
        };
        let fd = conn
            .data
            .as_ref()
            .map(|d| d.as_raw_fd())
            .ok_or_else(|| Error(format!("can't open {addr}/data")))?;
        return Ok((fd, Vec::new(), Some(conn)));
    }
    if let Some(cmd) = dest.strip_prefix('{').and_then(|c| c.strip_suffix('}')) {
        let [a, b] = purgatorio::pipe()?;
        purgatorio::spawn(
            "/bin/sh.wasm",
            &[String::from("sh"), String::from("-c"), String::from(cmd)],
            pctl::NEWFD,
            &[(a.as_raw_fd(), 0), (a.as_raw_fd(), 1), (2, 2)],
        )
        .map_err(|e| Error(format!("can't open {dest}: {e}")))?;
        drop(a);
        return Ok((b.as_raw_fd(), vec![b], None));
    }
    let f = purgatorio::open(dest, mode::ORDWR)
        .map_err(|e| Error(format!("can't open {dest}: {e}")))?;
    Ok((f.as_raw_fd(), vec![f], None))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--reader") => {
            let flags = args.get(2).map(String::as_str).unwrap_or("");
            return reader(flags.contains('v'), flags.contains('s'));
        }
        Some("--ticker") => return ticker(),
        _ => {}
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut net, mut server, mut verbose, mut msize) = (false, false, false, 64 * 1024);
    while let Some(c) = arg.opt() {
        match c {
            'n' => net = true,
            's' => server = true,
            'v' => verbose = true,
            'm' => {
                msize = arg
                    .earg()?
                    .parse::<usize>()
                    .map_err(|_| Error(format!("usage: {USAGE}")))?
            }
            _ => return Err(arg.usage()),
        }
    }
    let rest = arg.argv();
    if rest.len() > 1 {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let dest = rest.first().map(String::as_str);
    let input: Box<dyn Read> = if dest.is_none() {
        Box::new(
            File::open("/dev/cons")
                .map_err(|e| Error(format!("can't open /dev/cons: {}", Error::from(e))))?,
        )
    } else {
        Box::new(io::stdin())
    };
    let (conn, _keep, _netconn) = connect(dest, net, server)?;
    let [evr, evw] = purgatorio::pipe()?;
    let flags = format!(
        "-{}{}",
        if verbose { "v" } else { "" },
        if server { "s" } else { "" }
    );
    let rpid = purgatorio::spawn(
        "/bin/styxchat.wasm",
        &[String::from("styxchat"), String::from("--reader"), flags],
        pctl::NEWFD,
        &[(conn, 0), (evw.as_raw_fd(), EVFD), (1, 1), (2, 2)],
    )?;
    let tpid = purgatorio::spawn(
        "/bin/styxchat.wasm",
        &[String::from("styxchat"), String::from("--ticker")],
        pctl::NEWFD,
        &[(evw.as_raw_fd(), EVFD)],
    )?;
    drop(evw);
    // SAFETY: `conn` stays open for as long as `_keep` or `_netconn` do,
    // or is this prog's fd 0; the File is never closed (see below).
    let mut out = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(conn) });
    let mut ev = Events {
        pipe: File::from(evr),
        buf: Vec::new(),
        arrived: Vec::new(),
        hungup: false,
    };
    // Client: the tags sent and not yet answered. Server: the tags that
    // arrived and have not been answered.
    let mut due: BTreeSet<u16> = BTreeSet::new();
    let mut nexttag: u16 = 1;
    let mut stdout = io::stdout();
    for line in BufReader::new(input).lines() {
        let line = line?;
        let words = purgatorio::unquote_fields(&line);
        if words.is_empty() || words[0].starts_with('#') {
            continue;
        }
        let w = leak(words);
        let parsed = if server { rmsg(&w) } else { tmsg(&w, nexttag) };
        let (b, m) = match parsed {
            Err(e) => {
                let _ = writeln!(stdout, "?{e}");
                continue;
            }
            Ok(Line::Dump) => {
                verbose = true;
                continue;
            }
            Ok(Line::NextTag(Some(t))) => {
                nexttag = t;
                continue;
            }
            Ok(Line::NextTag(None)) => {
                let _ = writeln!(stdout, "?next tag is {nexttag}");
                continue;
            }
            Ok(Line::Send(b, m)) => (b, m),
        };
        if b.len() > msize {
            let _ = writeln!(stdout, "?message bigger than agreed: {} bytes", b.len());
            continue;
        }
        if server {
            // Answer only what has been asked.
            let t = m.tag;
            ev.wait(|arrived| {
                due.extend(arrived.drain(..));
                due.contains(&t)
            });
            due.remove(&t);
        }
        show("->", &m, verbose);
        if let Err(e) = out.write_all(&b) {
            let _ = writeln!(
                stdout,
                "?write error to {}: {}",
                if server { "clients" } else { "server" },
                Error::from(e)
            );
            continue;
        }
        if !server {
            if m.tag != NOTAG {
                nexttag = nexttag.wrapping_add(1);
                if nexttag == NOTAG {
                    nexttag = 0;
                }
            }
            due.insert(m.tag);
            let flushed = match m.fcall {
                Fcall::Tflush { oldtag } => Some(oldtag),
                _ => None,
            };
            let t = m.tag;
            ev.wait(|arrived| {
                for a in arrived.drain(..) {
                    due.remove(&a);
                    if a == t
                        && let Some(old) = flushed
                    {
                        due.remove(&old);
                    }
                }
                !due.contains(&t)
            });
        }
    }
    // The replies still on their way. A server has nothing more to say.
    if !server {
        ev.wait(|arrived| {
            for a in arrived.drain(..) {
                due.remove(&a);
            }
            due.is_empty()
        });
    }
    kill(tpid);
    kill(rpid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<&'static str> {
        leak(purgatorio::unquote_fields(s))
    }

    #[test]
    fn numbers_and_qids() {
        assert_eq!(num("16rff"), Some(255));
        assert_eq!(num("~0"), Some(u64::MAX));
        assert_eq!(fid("nofid"), Ok(NOFID));
        assert_eq!(tag("notag"), Ok(NOTAG));
        assert_eq!(
            qid("5.2.d"),
            Ok(Qid {
                path: 5,
                vers: 2,
                qtype: 0x80
            })
        );
        assert_eq!(qid("7").map(|q| q.qtype), Ok(0));
    }

    #[test]
    fn lines() {
        let Ok(Line::Send(_, m)) = tmsg(&w("Twalk 0 1 a 'b c'"), 4) else {
            panic!("walk");
        };
        assert_eq!(text::msg(&m), "Tmsg.Walk(4,0,1,array[] of {\"a\",\"b c\"})");
        let Ok(Line::Send(_, m)) = tmsg(&w("Tversion 8192 9P2000"), 4) else {
            panic!("version");
        };
        assert_eq!(m.tag, NOTAG);
        assert!(matches!(tmsg(&w("Topen 1"), 1), Err(e) if e == "usage: Topen fid mode"));
        assert!(matches!(tmsg(&w("Tbogus"), 1), Err(e) if e == "unknown message type"));
        let Ok(Line::Send(_, m)) = rmsg(&w("Rattach 1 0.0.d")) else {
            panic!("attach");
        };
        assert_eq!(text::msg(&m), "Rmsg.Attach(1,Qid(16r0,0,16r80))");
        assert!(matches!(rmsg(&w("Rwstat")), Err(e) if e == "usage: Rwstat tag"));
    }
}
