//! `#p`: one directory per prog. `wait` collects children's exit messages.
//! `status`, `ns`, `fd` and `module` describe the prog; writing `kill` or
//! `killgrp` to `ctl` interrupts it, and `modules warn` or `modules
//! require` tightens its module policy (`docs/signing.md`).
//!
//! `stack`, `exception` and `prof` are the debugging files
//! (`docs/debug.md`): the call stack of a traced prog, the trap that broke
//! it, and its profile. `trace on|off` and `prof on|off` on `ctl` set what
//! the prog's future children are started with.
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

/// The error of reading the `stack` of a prog started without tracing.
pub const ENOTTRACED: &str = "prog is not traced";

/// `(name, kind, perm)` of the files in each prog directory.
const FILES: &[(&str, u64, u32)] = &[
    ("ctl", QCTL, 0o222),
    ("exception", QEXCEPTION, 0o444),
    ("fd", QFD, 0o444),
    ("module", QMODULE, 0o444),
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
        Dir {
            dtype: 'p' as u16,
            qid: DevProg::qid(pid, kind),
            mode,
            name,
            uid: String::from(self.procs.user()),
            gid: String::from(self.procs.user()),
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
            _ => return Err(Error::new(error::EBADARG)),
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
        } else {
            access == mode::OREAD
        };
        if !ok {
            return Err(Error::new(error::EPERM));
        }
        self.get(c, pid)?;
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
        let text = match kind {
            QSTATUS => format!(
                "{} {} {} {} {} {}\n",
                p.pid,
                p.ppid,
                p.group(),
                self.procs.user(),
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
            QPROF => prof_text(&p),
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

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        let (pid, kind) = split(c);
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

/// A line per sampled function: `<samples> <function index> <path>`, for
/// the prog and for its children that have exited, ordered by path and
/// index.
fn prof_text(p: &crate::prog::Proc) -> String {
    let mut out = String::new();
    for ((path, func), n) in p.profile() {
        out.push_str(&format!("{n} {func} {}\n", crate::names::quote(&path)));
    }
    out
}
