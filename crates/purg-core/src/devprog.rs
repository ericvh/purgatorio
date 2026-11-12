//! `#p`: one directory per prog. `wait` collects children's exit messages.
//! `status`, `ns`, `fd` and `module` describe the prog; writing `kill` or
//! `killgrp` to `ctl` interrupts it, and `modules warn` or `modules
//! require` tightens its module policy (`docs/signing.md`). A prog's
//! files are its user's; only that user or the host owner may open its
//! `ctl` and `heap` (`docs/abi.md`, "Users").
//!
//! Memory limits (`docs/abi.md`): `memlimit N` on `ctl` limits the wasm
//! memory the prog holds (linear memory, and tables at
//! [`TABLE_ELEM_BYTES`](crate::prog::TABLE_ELEM_BYTES) an element) to `N`
//! bytes (`k`, `m` and `g` multiply by 1024s); the progs it starts later
//! inherit the limit. `domain memlimit N` and `domain proglimit N` limit
//! the innermost domain the prog is in: the memory its members hold
//! between them, and how many members it may have, past which a spawn
//! fails with `no free procs`. A limit can only be lowered: raising one
//! is `permission denied`. `mem` reads `<bytes held> <limit>`, the limit
//! being `-` for none.
//!
//! `stack`, `exception`, `prof`, `cprof`, `mprof` and `heap` are the
//! debugging files (`docs/debug.md`): the call stack of a traced prog, the
//! trap that broke it, its profiles (samples, calls, memory grown), and
//! its linear memory, readable and writable while a debugger holds it
//! stopped. `trace on|off`, `prof on|off`, `cprof on|off` and `mprof
//! on|off` on `ctl` set what the prog's future children are started with;
//! `stop`, `start`, `step`, `bpt set <func>` and `bpt del <func>` are the
//! debugger's.
//!
//! `#pdomain` is a restricted view, for sandboxes (`docs/abi.md`).
//! Attaching it founds a prog domain for the attaching prog's process
//! group: every prog in that group joins it, and every prog started from
//! a member afterwards is a member too, through any number of
//! generations, whatever its own group. The view shows only the domain's
//! members. Every other pid does not exist in it, so naming one fails
//! with `file does not exist`, and `killgrp` kills only the members of
//! the group. Domains nest: attaching `#pdomain` inside a domain founds
//! one within it, which the outer view still shows. Under `NODEVS`
//! nothing can attach `#p` at all, so a sandbox that binds `#pdomain` on
//! `/prog` and then gives up devices cannot get the full view back.
//!
//! `#pdomain=N` is the view of domain `N` (the process group that founded
//! it) without joining it: how the prog that started a sandbox finds every
//! prog left in it, whatever their groups, to kill them at the end. The
//! whole table is no secret from a prog that may attach `#p` at all, so
//! nothing more is checked.
//!
//! `nodev <chars>` on `ctl` forbids the prog's namespace, and every copy
//! made of it afterwards, to attach the devices named by `<chars>`, NODEVS
//! or not: a sandbox drops `#¤` that way.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::chan::Chan;
use crate::dev::{Dev, Walked};
use crate::error::{Error, Result};
use crate::prog::Procs;

const QROOT: u64 = 0;
const QDIR: u64 = 1;
const QWAIT: u64 = 2;
const QSTATUS: u64 = 3;
const QCTL: u64 = 4;
const QNS: u64 = 5;
const QFD: u64 = 6;
const QMODULE: u64 = 7;
const QSTACK: u64 = 8;
const QEXCEPTION: u64 = 9;
const QPROF: u64 = 10;
const QMEM: u64 = 11;
const QCPROF: u64 = 12;
const QMPROF: u64 = 13;
const QHEAP: u64 = 14;

use crate::debug::{Access, Kind, MAX_ACCESS, Resume};
pub use crate::debug::{ENOTSTOPPED, ENOTTRACED};

/// `(name, kind, perm)` of the files in each prog directory.
const FILES: &[(&str, u64, u32)] = &[
    ("cprof", QCPROF, 0o444),
    ("ctl", QCTL, 0o222),
    ("exception", QEXCEPTION, 0o444),
    ("fd", QFD, 0o444),
    ("heap", QHEAP, 0o600),
    ("mem", QMEM, 0o444),
    ("module", QMODULE, 0o444),
    ("mprof", QMPROF, 0o444),
    ("ns", QNS, 0o444),
    ("prof", QPROF, 0o444),
    ("stack", QSTACK, 0o444),
    ("status", QSTATUS, 0o444),
    ("wait", QWAIT, 0o444),
];

pub struct DevProg {
    procs: Arc<Procs>,
}

/// A `#pdomain` channel's view: the domain, named by the process group
/// that founded it.
struct View(u32);

/// The domain a channel is restricted to, `None` for the whole table.
fn view(c: &Chan) -> Option<u32> {
    c.aux::<View>().map(|v| v.0)
}

impl DevProg {
    pub fn new(procs: Arc<Procs>) -> DevProg {
        DevProg { procs }
    }

    /// Prog `pid`, when it exists and `c`'s view shows it.
    fn get(&self, c: &Chan, pid: u32) -> Result<Arc<crate::prog::Proc>> {
        let p = self.procs.get(pid).ok_or(Error::new(error::ENONEXIST))?;
        match view(c) {
            Some(d) if !p.in_domain(d) => Err(Error::new(error::ENONEXIST)),
            _ => Ok(p),
        }
    }

    fn qid(pid: u32, kind: u64) -> Qid {
        Qid {
            path: (u64::from(pid) << 4) | kind,
            vers: 0,
            qtype: if kind == QROOT || kind == QDIR {
                QTDIR
            } else {
                QTFILE
            },
        }
    }

    fn dir(&self, pid: u32, kind: u64) -> Dir {
        let (name, mode) = match kind {
            QROOT => (String::from("#p"), DMDIR | 0o555),
            QDIR => (format!("{pid}"), DMDIR | 0o555),
            _ => {
                let f = FILES.iter().find(|f| f.1 == kind).expect("known kind");
                (String::from(f.0), f.2)
            }
        };
        // A prog's files are its user's, as in Inferno.
        let user = match self.procs.get(pid) {
            Some(p) if kind != QROOT => p.user(),
            _ => String::from(self.procs.user()),
        };
        Dir {
            dtype: 'p' as u16,
            qid: DevProg::qid(pid, kind),
            mode,
            name,
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }
    }
}

fn split(c: &Chan) -> (u32, u64) {
    ((c.qid.path >> 4) as u32, c.qid.path & 0xf)
}

impl Dev for DevProg {
    fn dc(&self) -> char {
        'p'
    }

    fn name(&self) -> &'static str {
        "prog"
    }

    fn attach(&self, spec: &str) -> Result<Walked> {
        let aux: crate::chan::Aux = match spec {
            "" => None,
            "domain" => {
                let g = self.procs.current()?.group();
                // A prog its group spawns meanwhile may copy its parent's
                // domains before the parent joins; the progs that bind a
                // view (sh waiting on `bind`, a sandbox building itself)
                // start nothing while they do.
                for pid in self.procs.pids() {
                    if let Some(p) = self.procs.get(pid)
                        && p.group() == g
                    {
                        p.join_domain(g);
                    }
                }
                Some(Arc::new(View(g)))
            }
            _ => {
                let id = spec
                    .strip_prefix("domain=")
                    .and_then(|n| n.parse::<u32>().ok())
                    .ok_or(Error::new(error::EBADARG))?;
                Some(Arc::new(View(id)))
            }
        };
        Ok(Walked {
            qid: DevProg::qid(0, QROOT),
            aux,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let (pid, kind) = split(c);
        let qid = match kind {
            QROOT => {
                let pid: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                self.get(c, pid)?;
                DevProg::qid(pid, QDIR)
            }
            QDIR => {
                let f = FILES.iter().find(|f| f.0 == name);
                DevProg::qid(pid, f.ok_or(Error::new(error::ENONEXIST))?.1)
            }
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked {
            qid,
            aux: c.aux.clone(),
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        let (pid, kind) = split(c);
        Ok(self.dir(pid, kind))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let (pid, kind) = split(c);
        if kind == QROOT || kind == QDIR {
            return Ok(());
        }
        let access = omode & mode::ACCESS;
        let ok = if kind == QCTL {
            access == mode::OWRITE || access == mode::ORDWR
        } else if kind == QHEAP {
            access != mode::OEXEC
        } else {
            access == mode::OREAD
        };
        if !ok {
            return Err(Error::new(error::EPERM));
        }
        let p = self.get(c, pid)?;
        // Only its own user, or the host owner, controls a prog, or
        // touches its memory.
        if (kind == QCTL || kind == QHEAP)
            && !self.procs.is_owner()
            && p.user() != self.procs.current_user()
        {
            return Err(Error::new(error::EPERM));
        }
        Ok(())
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        let (pid, kind) = split(c);
        let p = self.get(c, pid)?;
        if kind == QWAIT {
            let msg = p.wait()?;
            let n = buf.len().min(msg.len());
            buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
            return Ok(n);
        }
        if kind == QHEAP {
            let len = buf.len().min(MAX_ACCESS);
            let data = p.debug.access(Access::Read { at: offset, len })?;
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            return Ok(n);
        }
        let text = match kind {
            QSTATUS => format!(
                "{} {} {} {} {} {}\n",
                p.pid,
                p.ppid,
                p.group(),
                p.user(),
                p.state(),
                p.name()
            ),
            QNS => p.pgrp().namespace(),
            QFD => fd_text(&p),
            QMODULE => module_text(&p),
            QSTACK => stack_text(&p)?,
            QEXCEPTION => match p.debug.exception() {
                e if e.is_empty() => e,
                e => format!("{e}\n"),
            },
            QPROF => counts_text(&p, Kind::Samples),
            QCPROF => counts_text(&p, Kind::Calls),
            QMPROF => counts_text(&p, Kind::Pages),
            QMEM => mem_text(&p),
            _ => return Err(Error::new(error::EPERM)),
        };
        Ok(crate::dev::read_str(text.as_bytes(), buf, offset))
    }

    /// A `wait` file is readable when an exit message is queued; the other
    /// files never block.
    fn ready(&self, c: &Chan, write: bool) -> bool {
        let (pid, kind) = split(c);
        if kind != QWAIT || write {
            return true;
        }
        self.procs.get(pid).is_none_or(|p| p.wait_ready())
    }

    fn watch(&self, c: &Chan, write: bool, w: &purg_platform::sync::Rendez) {
        let (pid, kind) = split(c);
        if kind == QWAIT
            && !write
            && let Some(p) = self.procs.get(pid)
        {
            p.watch_wait(w);
        }
    }

    fn unwatch(&self, c: &Chan, w: &purg_platform::sync::Rendez) {
        let (pid, kind) = split(c);
        if kind == QWAIT
            && let Some(p) = self.procs.get(pid)
        {
            p.unwatch_wait(w);
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let (pid, kind) = split(c);
        Ok(match kind {
            QROOT => self
                .procs
                .pids()
                .into_iter()
                .filter(|&p| self.get(c, p).is_ok())
                .map(|p| self.dir(p, QDIR))
                .collect(),
            _ => FILES.iter().map(|f| self.dir(pid, f.1)).collect(),
        })
    }

    fn write(&self, c: &Chan, buf: &[u8], offset: u64) -> Result<usize> {
        let (pid, kind) = split(c);
        if kind == QHEAP {
            let p = self.get(c, pid)?;
            if buf.len() > MAX_ACCESS {
                return Err(Error::new(error::EBADARG));
            }
            p.debug.access(Access::Write {
                at: offset,
                data: buf.to_vec(),
            })?;
            return Ok(buf.len());
        }
        if kind != QCTL {
            return Err(Error::new(error::EPERM));
        }
        let p = self.get(c, pid)?;
        let cmd = core::str::from_utf8(buf)
            .map_err(|_| Error::new(error::EBADARG))?
            .trim();
        match cmd {
            "kill" => self.procs.kill(pid),
            "killgrp" => match view(c) {
                None => self.procs.kill_group(p.group()),
                Some(d) => {
                    for pid in self.procs.pids() {
                        if let Some(q) = self.procs.get(pid)
                            && q.group() == p.group()
                            && q.in_domain(d)
                        {
                            self.procs.kill(pid);
                        }
                    }
                }
            },
            "trace on" => p.debug.set_trace(true),
            "trace off" => p.debug.set_trace(false),
            "prof on" => p.debug.set_prof(true),
            "prof off" => p.debug.set_prof(false),
            "cprof on" => p.debug.set_collect(Kind::Calls, true),
            "cprof off" => p.debug.set_collect(Kind::Calls, false),
            "mprof on" => p.debug.set_collect(Kind::Pages, true),
            "mprof off" => p.debug.set_collect(Kind::Pages, false),
            "stop" => p.debug.request_stop()?,
            "start" => p.debug.resume(Resume::Cont)?,
            "step" => p.debug.resume(Resume::Step)?,
            _ if cmd.starts_with("bpt ") => {
                let mut w = cmd.split_ascii_whitespace().skip(1);
                let on = match w.next() {
                    Some("set") => true,
                    Some("del") => false,
                    _ => return Err(Error::new(error::EBADARG)),
                };
                let func = w
                    .next()
                    .and_then(|f| f.parse::<u32>().ok())
                    .ok_or(Error::new(error::EBADARG))?;
                if w.next().is_some() {
                    return Err(Error::new(error::EBADARG));
                }
                p.debug.set_break(func, on)?;
            }
            _ if cmd.starts_with("memlimit ") || cmd.starts_with("domain ") => {
                self.limit(&p, cmd)?
            }
            _ if cmd.starts_with("nodev ") => {
                let devs = cmd["nodev ".len()..].trim();
                if devs.is_empty() || devs.contains(char::is_whitespace) {
                    return Err(Error::new(error::EBADARG));
                }
                p.pgrp().deny_devs(devs);
            }
            _ => {
                let level = cmd
                    .strip_prefix("modules ")
                    .and_then(|l| crate::modsig::Level::parse(l.trim()))
                    .ok_or(Error::new(error::EBADARG))?;
                p.tighten_module_policy(level)?;
            }
        }
        Ok(buf.len())
    }
}

impl DevProg {
    /// `memlimit N`, `domain memlimit N` or `domain proglimit N`.
    fn limit(&self, p: &crate::prog::Proc, cmd: &str) -> Result<()> {
        let bad = || Error::new(error::EBADARG);
        let mut words = cmd.split_ascii_whitespace();
        let (domain, what) = match words.next() {
            Some("domain") => (true, words.next().ok_or_else(bad)?),
            Some(w) => (false, w),
            None => return Err(bad()),
        };
        let n = words.next().and_then(parse_size).ok_or_else(bad)?;
        if words.next().is_some() {
            return Err(bad());
        }
        match (domain, what) {
            (false, "memlimit") => p.set_memlimit(n),
            (true, "memlimit" | "proglimit") => {
                let d = *p.domains().last().ok_or_else(bad)?;
                let mut l = self.procs.domain_limits(d);
                if what == "memlimit" {
                    l.mem = n;
                } else {
                    l.progs = n;
                }
                self.procs.set_domain_limits(d, l)
            }
            _ => Err(bad()),
        }
    }
}

/// A count of bytes or progs: digits, then `k`, `m` or `g` (either case)
/// for 2^10, 2^20 or 2^30 times as many.
pub fn parse_size(s: &str) -> Option<u64> {
    let (digits, shift) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 10),
        b'm' | b'M' => (&s[..s.len() - 1], 20),
        b'g' | b'G' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()?.checked_mul(1 << shift)
}

/// `<bytes held> <limit>`, with `-` for no limit.
fn mem_text(p: &crate::prog::Proc) -> String {
    match p.memlimit() {
        crate::prog::UNLIMITED => format!("{} -\n", p.mem()),
        l => format!("{} {l}\n", p.mem()),
    }
}

fn fd_text(p: &crate::prog::Proc) -> String {
    use purg_abi::mode;
    let fgrp = p.fgrp();
    let mut out = String::new();
    for fd in fgrp.fds() {
        let Ok(c) = fgrp.get(fd) else { continue };
        let acc = match c.mode & mode::ACCESS {
            mode::OWRITE => "w",
            mode::ORDWR => "rw",
            mode::OEXEC => "x",
            _ => "r",
        };
        out.push_str(&format!("{fd} {acc} {}\n", c.name));
    }
    out
}

/// `policy <level>`, then a line per loaded module: `module <digest>
/// <signers> <path>`, with `-` when no signer was verified.
fn module_text(p: &crate::prog::Proc) -> String {
    use purg_auth::hex;
    let mut out = format!("policy {}\n", p.module_policy().level.as_str());
    for m in p.modules() {
        let signers = if m.signers.is_empty() {
            String::from("-")
        } else {
            let keys: Vec<String> = m.signers.iter().map(|k| hex::encode(k)).collect();
            keys.join(",")
        };
        out.push_str(&format!(
            "module {} {signers} {}\n",
            hex::encode(&m.digest),
            crate::names::quote(&m.path)
        ));
    }
    out
}

/// A line per frame, the innermost first: `<function index> <path>`, the
/// path being the prog's command module, quoted when it holds white space or a quote.
/// When the prog had more frames than a snapshot keeps, a last line
/// `... <n>` counts the outer frames left out.
fn stack_text(p: &crate::prog::Proc) -> Result<String> {
    if !p.debug.traced() {
        return Err(Error::new(ENOTTRACED));
    }
    let stack = p.debug.stack();
    let path = crate::names::quote(&p.module_path());
    let mut out = String::new();
    for f in &stack.frames {
        out.push_str(&format!("{f} {path}\n"));
    }
    let more = (stack.depth as usize).saturating_sub(stack.frames.len());
    if more > 0 {
        out.push_str(&format!("... {more}\n"));
    }
    Ok(out)
}

/// A line per function counted: `<count> <function index> <path>`, for
/// the prog and for its children that have exited, ordered by path and
/// index. What is counted is `kind`: samples (`prof`), calls (`cprof`) or
/// pages grown (`mprof`).
fn counts_text(p: &crate::prog::Proc, kind: Kind) -> String {
    let mut out = String::new();
    for ((path, func), n) in p.debug.counts(kind, &p.module_path()) {
        out.push_str(&format!("{n} {func} {}\n", crate::names::quote(&path)));
    }
    out
}
