//! Running parsed commands. Builtins, assignments and blocks run inside
//! this prog; everything else is a command spawned as a new prog. A stage
//! of a pipeline or a background job that is not a plain command runs in a
//! sub-shell (`sh -c` with its source), as Inferno's sh forks for one.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use purgatorio::abi::{mode, pctl};
use purgatorio::{Error, Waitmsg};

use crate::lex::{Part, RedirKind, VarKind, Word, is_blank};
use crate::modules::{Loaded, Module};
use crate::parse::{self, AndOr, Cmd, CmdKind, Item, Pipeline, Redir};
use crate::shmods::Fail;

pub const SH: &str = "/bin/sh.wasm";

/// Writes `msg` to the `ctl` file of prog `pid`.
fn ctl(pid: u32, msg: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/prog/{pid}/ctl"))?
        .write_all(msg.as_bytes())
}

/// Variables that are never written to `/env`.
fn exported(name: &str) -> bool {
    !(name == "*" || name == "status" || name == "apid" || name.chars().all(|c| c.is_ascii_digit()))
}

/// The fds a command sees, as `(its fd, this prog's fd)`.
#[derive(Clone)]
pub struct Io {
    map: Vec<(i32, i32)>,
}

impl Io {
    pub fn std() -> Io {
        Io {
            map: vec![(0, 0), (1, 1), (2, 2)],
        }
    }

    pub fn get(&self, fd: i32) -> Option<i32> {
        self.map.iter().find(|m| m.0 == fd).map(|m| m.1)
    }

    pub fn set(&mut self, fd: i32, to: i32) {
        self.map.retain(|m| m.0 != fd);
        self.map.push((fd, to));
    }

    pub fn fdmap(&self) -> Vec<(i32, i32)> {
        self.map
            .iter()
            .map(|&(child, ours)| (ours, child))
            .collect()
    }

    pub fn write(&self, fd: i32, s: &str) {
        let _ = self.try_write(fd, s.as_bytes());
    }

    /// Writes all of `b` to the command's fd `fd`.
    pub fn try_write(&self, fd: i32, b: &[u8]) -> std::io::Result<()> {
        let Some(fd) = self.get(fd) else {
            return Err(std::io::Error::other("bad fd number"));
        };
        // SAFETY: `fd` is open in this prog for the duration of the
        // command, and ManuallyDrop keeps the File from closing it.
        let f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        (&*f).write_all(b)
    }
}

/// Quotes a word for a sub-shell or `/env` when it needs it.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| is_blank(c) || "'#;&|{}()<>$`^=".contains(c))
    {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

pub struct Shell {
    pub(crate) vars: HashMap<String, Vec<String>>,
    pub(crate) locals: HashSet<String>,
    /// Background progs not yet seen to exit.
    background: HashSet<u32>,
    /// Exit messages for progs not yet waited for.
    exited: HashMap<u32, String>,
    pub status: String,
    /// Modules from `load`, in the order they were loaded. A later module's
    /// names hide an earlier one's.
    pub(crate) loaded: Vec<Loaded>,
    /// `mload` namespaces: a name and the modules loaded into it.
    pub(crate) namespaces: Vec<(String, Vec<Loaded>)>,
    /// Function bodies from `fn name {body}`, including the braces.
    fns: HashMap<String, String>,
    /// The exception a failed `${}` builtin raised, for the code that
    /// reports the expansion error.
    raised: Option<String>,
    /// `load test`: where `report` sends messages.
    pub(crate) test: Option<crate::shmods::TestConf>,
    /// The outstanding requests of a `file2chan` server.
    pub(crate) tags: crate::f2c::Tags,
    /// Running the `-P` prelude of a sub-shell: `load` stays quiet.
    pub(crate) quiet: bool,
    /// `-e`: a command that fails in the middle of a sequence raises its
    /// status, so the script stops there.
    pub errexit: bool,
    /// An interactive shell survives the console's interrupt
    /// ([`Shell::catch_interrupts`]).
    catchint: bool,
    /// An interrupt came while a command ran: the rest of the input being
    /// run is abandoned.
    pub interrupted: bool,
}

/// Why execution stopped early.
pub enum Stop {
    /// `exit`: leave the shell.
    Exit(String),
    /// `raise`: abandon the current script with this status.
    Raise(String),
}

pub(crate) type Flow<T> = Result<T, Stop>;

impl Shell {
    pub fn new(argv0: &str, args: Vec<String>) -> Shell {
        let mut vars = HashMap::new();
        for (k, v) in std::env::vars() {
            vars.insert(k, vec![v]);
        }
        vars.insert(String::from("0"), vec![String::from(argv0)]);
        vars.insert(String::from("*"), args);
        Shell {
            vars,
            locals: HashSet::new(),
            background: HashSet::new(),
            exited: HashMap::new(),
            status: String::new(),
            loaded: Vec::new(),
            namespaces: Vec::new(),
            fns: HashMap::new(),
            raised: None,
            test: None,
            tags: Default::default(),
            quiet: false,
            errexit: false,
            catchint: false,
            interrupted: false,
        }
    }

    /// For an interactive shell: a process group of its own, so that the
    /// console's interrupt key reaches it and the commands it starts but
    /// not the prog that started it, and `catchint on`, so that it
    /// survives the `interrupt` note (`docs/abi.md`, "Interrupts").
    pub fn catch_interrupts(&mut self) {
        let _ = purgatorio::pctl(pctl::NEWPGRP, &[]);
        self.catchint = ctl(purgatorio::pid(), "catchint on").is_ok();
    }

    pub(crate) fn set_var(&mut self, name: &str, value: Vec<String>, local: bool) {
        if local {
            self.locals.insert(String::from(name));
        }
        if !local && !self.locals.contains(name) && exported(name) {
            let res = if value.is_empty() {
                purgatorio::env::unset(name)
            } else if value.len() == 1 {
                purgatorio::env::set(name, &value[0])
            } else {
                let joined: Vec<String> = value.iter().map(|v| quote(v)).collect();
                purgatorio::env::set(name, &joined.join(" "))
            };
            if let Err(e) = res {
                eprintln!("sh: {name}: {e}");
            }
        }
        self.vars.insert(String::from(name), value);
    }

    pub(crate) fn var(&self, name: &str) -> Vec<String> {
        if let Ok(n) = name.parse::<usize>()
            && n > 0
        {
            return self
                .vars
                .get("*")
                .and_then(|a| a.get(n - 1))
                .cloned()
                .into_iter()
                .collect();
        }
        if name == "status" {
            return vec![self.status.clone()];
        }
        self.vars.get(name).cloned().unwrap_or_default()
    }

    /// Runs a whole script, stopping only for `exit`.
    pub fn run_src(&mut self, src: &str) -> Flow<()> {
        match parse::parse(src) {
            Ok(items) => self.run_items(&items, &Io::std()),
            Err(crate::lex::SyntaxError::Incomplete) => {
                self.fail("syntax error: unexpected end of input");
                Ok(())
            }
            Err(crate::lex::SyntaxError::Bad(m)) => {
                self.fail(&m);
                Ok(())
            }
        }
    }

    fn fail(&mut self, msg: &str) {
        eprintln!("sh: {msg}");
        self.status = format!("fail:{msg}");
    }

    fn run_items(&mut self, items: &[Item], io: &Io) -> Flow<()> {
        for (n, item) in items.iter().enumerate() {
            if item.background {
                self.background_item(item, io);
            } else {
                self.status = self.andor(&item.andor, io)?;
                self.check_interrupt()?;
                // As Inferno's sh, -e checks each command of a sequence
                // but the last, whose status is the sequence's own.
                if self.errexit && !self.status.is_empty() && n + 1 < items.len() {
                    return Err(Stop::Raise(self.status.clone()));
                }
            }
        }
        Ok(())
    }

    fn andor(&mut self, a: &AndOr, io: &Io) -> Flow<String> {
        let mut status = self.pipeline(&a.first, io)?;
        for (and, p) in &a.rest {
            self.check_interrupt()?;
            if status.is_empty() == *and {
                self.status = status;
                status = self.pipeline(p, io)?;
            }
        }
        Ok(status)
    }

    fn pipeline(&mut self, p: &Pipeline, io: &Io) -> Flow<String> {
        let status = if let [cmd] = p.cmds.as_slice()
            && self.runs_here(cmd)
        {
            self.here(cmd, io)?
        } else {
            match self.start_pipeline(&p.cmds, io) {
                Ok(pids) => {
                    let statuses = self.wait_for(&pids);
                    let failed: Vec<&str> = statuses
                        .iter()
                        .map(String::as_str)
                        .filter(|s| !s.is_empty())
                        .collect();
                    failed.join("|")
                }
                Err(e) => self.cmd_err(e, io)?,
            }
        };
        Ok(match (p.bang, status.is_empty()) {
            (false, _) => status,
            (true, true) => String::from("false"),
            (true, false) => String::new(),
        })
    }

    fn background_item(&mut self, item: &Item, io: &Io) {
        let a = &item.andor;
        let started = if a.rest.is_empty()
            && !a.first.bang
            && a.first.cmds.iter().all(|c| !self.runs_here(c))
        {
            self.start_pipeline(&a.first.cmds, io)
        } else {
            self.sub_shell(&item.src, io).map(|pid| vec![pid])
        };
        match started {
            Ok(pids) => {
                self.background.extend(&pids);
                if let Some(last) = pids.last() {
                    self.vars
                        .insert(String::from("apid"), vec![last.to_string()]);
                }
            }
            Err(e) => {
                self.raised = None;
                self.fail(&e.0);
            }
        }
    }

    /// Starts every stage of a pipeline, each with its output piped to the
    /// next stage's input, and returns their pids.
    fn start_pipeline(&mut self, cmds: &[Cmd], io: &Io) -> Result<Vec<u32>, Error> {
        let mut pids = Vec::new();
        let mut input: Option<OwnedFd> = None;
        for (i, cmd) in cmds.iter().enumerate() {
            let mut stage = io.clone();
            if let Some(r) = &input {
                stage.set(0, r.as_raw_fd());
            }
            let pipe = if i + 1 < cmds.len() {
                let [r, w] = purgatorio::pipe()?;
                stage.set(1, w.as_raw_fd());
                Some((r, w))
            } else {
                None
            };
            let started = if self.runs_here(cmd) {
                self.sub_shell(&cmd.src, &stage)
            } else {
                self.spawn_simple(cmd, &stage)
            };
            // This prog keeps no pipe ends but the next stage's input, so
            // readers see end of file when the writers exit.
            input = pipe.map(|(r, _w)| r);
            match started {
                Ok(pid) => pids.push(pid),
                Err(e) => {
                    self.wait_for(&pids);
                    return Err(e);
                }
            }
        }
        Ok(pids)
    }

    fn sub_shell(&mut self, src: &str, io: &Io) -> Result<u32, Error> {
        let mut argv = self.sh_argv();
        argv.extend([String::from("-c"), String::from(src)]);
        purgatorio::spawn(SH, &argv, pctl::NEWFD, &io.fdmap())
    }

    /// The start of the argument list for a sub-shell: `sh`, then `-P`
    /// and a prelude that loads this shell's modules and defines its
    /// functions, if it has any, as Inferno's sh shares them with the
    /// shells it forks.
    pub(crate) fn sh_argv(&self) -> Vec<String> {
        self.sh_argv_with(false)
    }

    /// As `sh_argv`; with `locals`, the prelude also sets this shell's
    /// local variables, as `@` gives its sub-shell a copy of them.
    fn sh_argv_with(&self, locals: bool) -> Vec<String> {
        let mut prelude = String::new();
        if !self.loaded.is_empty() {
            let names: Vec<String> = self.loaded.iter().map(|l| quote(l.name())).collect();
            prelude.push_str(&format!("load {}\n", names.join(" ")));
        }
        for (ns, mods) in &self.namespaces {
            let names: Vec<String> = mods.iter().map(|l| quote(l.name())).collect();
            prelude.push_str(&format!("mload {} {}\n", quote(ns), names.join(" ")));
        }
        let mut fns: Vec<(&String, &String)> = self.fns.iter().collect();
        fns.sort();
        for (name, body) in fns {
            prelude.push_str(&format!("fn {} {body}\n", quote(name)));
        }
        if locals {
            let mut names: Vec<&String> = self.locals.iter().collect();
            names.sort();
            for name in names {
                let value: Vec<String> = self.var(name).iter().map(|v| quote(v)).collect();
                prelude.push_str(&format!("{name} := {}\n", value.join(" ")));
            }
        }
        let mut argv = vec![String::from("sh")];
        if self.errexit {
            argv.push(String::from("-e"));
        }
        if !prelude.is_empty() {
            argv.extend([String::from("-P"), prelude]);
        }
        argv
    }

    /// `@ command`: runs `command` in a sub-shell, a new prog with copies
    /// of this shell's variables, fds, environment and namespace, so `cd`,
    /// assignments, `load`, `bind` and `mount` in it do not change this
    /// shell, and waits for it. Its status is the command's.
    fn subshell(&mut self, argv: &[String], io: &Io) -> Result<String, Error> {
        if argv.len() < 2 {
            return Err(Error::from("usage: @ command"));
        }
        let words: Vec<String> = argv[1..]
            .iter()
            .map(|w| match w.starts_with('{') && w.ends_with('}') {
                true => w.clone(),
                false => quote(w),
            })
            .collect();
        let mut args = self.sh_argv_with(true);
        args.extend([String::from("-c"), words.join(" "), String::from("--")]);
        args.extend(self.var("*"));
        let flags = pctl::NEWFD | pctl::FORKNS | pctl::FORKENV;
        let pid = purgatorio::spawn(SH, &args, flags, &io.fdmap())?;
        Ok(self.wait_for(&[pid]).pop().unwrap_or_default())
    }

    fn spawn_simple(&mut self, cmd: &Cmd, io: &Io) -> Result<u32, Error> {
        let CmdKind::Simple(words) = &cmd.kind else {
            unreachable!("only simple commands are spawned directly");
        };
        let argv = self.expand_all(words)?;
        let Some(name) = argv.first() else {
            return Err(Error::from("empty command"));
        };
        let mut io = io.clone();
        let _files = self.redirect(&cmd.redirs, &mut io)?;
        let path = resolve(name)?;
        purgatorio::spawn(&path, &argv, pctl::NEWFD, &io.fdmap())
    }

    /// An interrupt came while a command ran: abandon what is being run.
    fn check_interrupt(&mut self) -> Flow<()> {
        if self.interrupted {
            return Err(Stop::Raise(String::from("interrupt")));
        }
        Ok(())
    }

    /// Waits until every prog in `pids` has exited and returns their
    /// statuses, in order. Messages from other children are kept.
    ///
    /// An interrupt while waiting is passed on to the progs not yet
    /// exited, which may have started after the console's note went to
    /// the process group, or be in a group of their own, and the waiting
    /// goes on: those that do not catch it die, and an interactive shell
    /// among them gives its prompt back.
    fn wait_for(&mut self, pids: &[u32]) -> Vec<String> {
        while !pids.iter().all(|p| self.exited.contains_key(p)) {
            match purgatorio::wait() {
                Ok(Waitmsg { pid, status, .. }) => {
                    if !self.background.remove(&pid) {
                        self.exited.insert(pid, status);
                    }
                }
                Err(e) if self.catchint && e.0 == purgatorio::abi::error::EINTR => {
                    self.interrupted = true;
                    for &p in pids.iter().filter(|p| !self.exited.contains_key(p)) {
                        let _ = ctl(p, "interrupt");
                    }
                }
                Err(e) => {
                    eprintln!("sh: wait: {e}");
                    break;
                }
            }
        }
        pids.iter()
            .map(|p| {
                self.exited
                    .remove(p)
                    .unwrap_or_else(|| String::from("fail:lost"))
            })
            .collect()
    }

    /// Reports an error from expanding or starting a command. A `${}`
    /// builtin that raised an exception has its message printed as is and
    /// the exception raised; any other error fails the command.
    fn cmd_err(&mut self, e: Error, io: &Io) -> Flow<String> {
        if let Some(name) = self.raised.take() {
            io.write(2, &format!("{e}\n"));
            return Err(Stop::Raise(name));
        }
        io.write(2, &format!("sh: {e}\n"));
        Ok(format!("fail:{e}"))
    }

    /// Runs a builtin, assignment or block in this prog.
    fn here(&mut self, cmd: &Cmd, outer: &Io) -> Flow<String> {
        let mut io = outer.clone();
        let _files = match self.redirect(&cmd.redirs, &mut io) {
            Ok(f) => f,
            // The files a failed list of redirections had opened are
            // closed again, so `io` may name fds that are gone: the error
            // goes where the shell's errors went before the command.
            Err(e) => return self.cmd_err(e, outer),
        };
        match &cmd.kind {
            CmdKind::Assign { name, local, value } => match self.expand_all(value) {
                Ok(v) => {
                    self.set_var(name, v, *local);
                    Ok(String::new())
                }
                Err(e) => self.cmd_err(e, &io),
            },
            CmdKind::Block(items) => {
                self.run_items(items, &io)?;
                Ok(self.status.clone())
            }
            CmdKind::Simple(words) => {
                let argv = match self.expand_all(words) {
                    Ok(a) => a,
                    Err(e) => return self.cmd_err(e, &io),
                };
                if argv.is_empty() {
                    return Ok(String::new());
                }
                self.run_argv(&argv, &io)
            }
        }
    }

    /// Runs a function, a module's command or a builtin.
    pub(crate) fn run_argv(&mut self, argv: &[String], io: &Io) -> Flow<String> {
        if let Some(body) = self.fns.get(&argv[0]).cloned() {
            return self.call_fn(&body, argv);
        }
        if let Some(m) = self.cmd_module(&argv[0]) {
            return self.mod_cmd(m, argv, io);
        }
        if self.ns_has(&argv[0], true) {
            return self.ns_cmd(argv, io);
        }
        self.builtin(argv, io)
    }

    fn builtin(&mut self, argv: &[String], io: &Io) -> Flow<String> {
        let res: Result<(), Error> = match argv[0].as_str() {
            "cd" => {
                let dir = match argv.get(1) {
                    Some(d) => d.clone(),
                    None => self
                        .var("home")
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "/".into()),
                };
                purgatorio::chdir(&dir).map_err(|e| Error(format!("cd: {dir}: {e}")))
            }
            "exit" => return Err(Stop::Exit(argv[1..].join(" "))),
            "run" => match argv.get(1) {
                Some(file) => match std::fs::read_to_string(file) {
                    Ok(src) => {
                        let saved = self.vars.insert(String::from("*"), argv[2..].to_vec());
                        let r = self.run_src(&src);
                        if let Some(s) = saved {
                            self.vars.insert(String::from("*"), s);
                        }
                        r?;
                        return Ok(self.status.clone());
                    }
                    Err(e) => Err(Error(format!("run: {file}: {}", Error::from(e)))),
                },
                None => Err(Error::from("usage: run file [args ...]")),
            },
            "wait" => {
                while !self.background.is_empty() {
                    match purgatorio::wait() {
                        Ok(m) => {
                            self.background.remove(&m.pid);
                        }
                        Err(_) => break,
                    }
                }
                Ok(())
            }
            "load" => self.load(&argv[1..], io),
            "@" => match self.subshell(argv, io) {
                Ok(status) => return Ok(status),
                Err(e) => Err(e),
            },
            _ => unreachable!("not a builtin"),
        };
        Ok(match res {
            Ok(()) => String::new(),
            Err(e) => {
                io.write(2, &format!("{e}\n"));
                format!("fail:{e}")
            }
        })
    }

    /// Opens redirection targets and points `io` at them. The returned fds
    /// must outlive the command's use of `io`.
    fn redirect(&mut self, redirs: &[Redir], io: &mut Io) -> Result<Vec<OwnedFd>, Error> {
        let mut files = Vec::new();
        for r in redirs {
            match r.kind {
                RedirKind::Dup(from) => {
                    let to = io
                        .get(from)
                        .ok_or_else(|| Error::from("bad fd in redirection"))?;
                    io.set(r.fd, to);
                    continue;
                }
                RedirKind::Close => {
                    io.map.retain(|m| m.0 != r.fd);
                    continue;
                }
                _ => {}
            }
            let target = r.target.as_ref().expect("file redirections have targets");
            let names = self.expand(target)?;
            let [name] = names.as_slice() else {
                return Err(Error::from("redirection needs one file name"));
            };
            let fd = match r.kind {
                RedirKind::In => purgatorio::open(name, mode::OREAD),
                RedirKind::RdWr => purgatorio::open(name, mode::ORDWR),
                RedirKind::Out => purgatorio::create(name, mode::OWRITE, 0o666),
                RedirKind::Append => match purgatorio::open(name, mode::OWRITE) {
                    Ok(fd) => {
                        let mut f = File::from(fd);
                        f.seek(SeekFrom::End(0))?;
                        Ok(OwnedFd::from(f))
                    }
                    Err(_) => purgatorio::create(name, mode::OWRITE, 0o666),
                },
                RedirKind::Dup(_) | RedirKind::Close => unreachable!("handled above"),
            }
            .map_err(|e| Error(format!("{name}: {e}")))?;
            io.set(r.fd, fd.as_raw_fd());
            files.push(fd);
        }
        Ok(files)
    }

    fn expand_all(&mut self, words: &[Word]) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        for w in words {
            out.extend(self.expand(w)?);
        }
        Ok(out)
    }

    /// The list a word stands for, with the file names its patterns match.
    fn expand(&mut self, w: &Word) -> Result<Vec<String>, Error> {
        let words = self.expand_raw(w)?;
        Ok(words.into_iter().flat_map(Glob::names).collect())
    }

    /// The list a word stands for, before file name patterns are expanded:
    /// that waits until the whole word has been put together.
    fn expand_raw(&mut self, w: &Word) -> Result<Vec<Glob>, Error> {
        let mut acc: Option<Vec<Glob>> = None;
        for p in &w.parts {
            let v = match p {
                Part::Lit(s, quoted) => vec![Glob::lit(s, *quoted)],
                Part::Var(name, kind) => {
                    let v = self.var(name);
                    Glob::plain(match kind {
                        VarKind::Plain => v,
                        VarKind::Count => vec![v.len().to_string()],
                        VarKind::Join => vec![v.join(" ")],
                    })
                }
                Part::Subst(src) => Glob::plain(self.substitute(src)?),
                Part::Builtin(src) => Glob::plain(self.builtin_subst(src)?),
                Part::List(words) => {
                    let mut l = Vec::new();
                    for w in words {
                        l.extend(self.expand_raw(w)?);
                    }
                    l
                }
            };
            acc = Some(match acc {
                None => v,
                Some(a) => concat(a, v)?,
            });
        }
        Ok(acc.unwrap_or_default())
    }

    /// `` `{src} ``: the words a sub-shell running `src` writes.
    fn substitute(&mut self, src: &str) -> Result<Vec<String>, Error> {
        let [r, w] = purgatorio::pipe()?;
        let mut io = Io::std();
        io.set(1, w.as_raw_fd());
        let pid = self.sub_shell(src, &io)?;
        drop(w);
        let mut out = String::new();
        let res = File::from(r).read_to_string(&mut out);
        self.status = self.wait_for(&[pid]).pop().unwrap_or_default();
        res?;
        Ok(fields(&out))
    }

    fn runs_here(&self, cmd: &Cmd) -> bool {
        match &cmd.kind {
            CmdKind::Simple(words) => words
                .first()
                .and_then(head_name)
                .is_some_and(|n| self.runs_name(n)),
            CmdKind::Assign { .. } | CmdKind::Block(_) => true,
        }
    }

    fn runs_name(&self, name: &str) -> bool {
        is_builtin(name)
            || self.fns.contains_key(name)
            || self.cmd_module(name).is_some()
            || self.ns_has(name, true)
    }

    /// The most recently loaded module that has command `name`.
    fn cmd_module(&self, name: &str) -> Option<Module> {
        self.loaded.iter().rev().find_map(|l| match l {
            Loaded::Builtin(m) if m.cmds().contains(&name) => Some(*m),
            _ => None,
        })
    }

    /// Whether `name` is an `mload` namespace with commands (`cmd`) or
    /// with `${}` builtins.
    fn ns_has(&self, name: &str, cmd: bool) -> bool {
        self.namespaces.iter().any(|(n, mods)| {
            n == name
                && mods.iter().any(|l| match l {
                    Loaded::Builtin(m) if cmd => !m.cmds().is_empty(),
                    Loaded::Builtin(m) => !m.substs().is_empty(),
                    Loaded::Reactor(..) => !cmd,
                })
        })
    }

    /// Finds module `name`: one compiled into sh, or a reactor from
    /// `/bin/lib`.
    pub(crate) fn find_module(&self, name: &str) -> Result<Loaded, Error> {
        if let Some(m) = Module::by_name(name) {
            return Ok(Loaded::Builtin(m));
        }
        if let Some(l) = self.loaded.iter().find(|l| l.name() == name) {
            return Ok(l.clone());
        }
        let path = format!("/bin/lib/{name}.wasm");
        let handle = purgatorio::load(&path).map_err(|e| Error(format!("load: {name}: {e}")))?;
        Ok(Loaded::Reactor(String::from(name), handle))
    }

    fn load(&mut self, names: &[String], io: &Io) -> Result<(), Error> {
        if names.is_empty() {
            return Err(Error::from("usage: load module ..."));
        }
        for name in names {
            if self.loaded.iter().any(|l| l.name() == name) {
                continue;
            }
            let l = self.find_module(name)?;
            if l == Loaded::Builtin(Module::Test) {
                self.test_init(io);
            }
            self.loaded.push(l);
        }
        Ok(())
    }

    /// `${...}`: run one loaded builtin and substitute its result.
    fn builtin_subst(&mut self, src: &str) -> Result<Vec<String>, Error> {
        let items = parse::parse(src).map_err(|e| Error(format!("syntax error: {e:?}")))?;
        let [item] = items.as_slice() else {
            return Err(Error::from("builtin must be one command"));
        };
        let [cmd] = item.andor.first.cmds.as_slice() else {
            return Err(Error::from("builtin must be one command"));
        };
        let CmdKind::Simple(words) = &cmd.kind else {
            return Err(Error::from("builtin must be one command"));
        };
        let argv = self.expand_all(words)?;
        if argv.is_empty() {
            return Err(Error::from("empty builtin"));
        }
        self.subst_argv(&argv).map_err(|f| {
            self.raised = f.name;
            Error(f.msg)
        })
    }

    /// Runs `${}` builtin `argv[0]` from the most recently loaded module
    /// that has it, or from an `mload` namespace.
    pub(crate) fn subst_argv(&mut self, argv: &[String]) -> Result<Vec<String>, Fail> {
        let name = argv[0].as_str();
        if let Some(l) = self
            .loaded
            .iter()
            .rev()
            .find(|l| l.has_subst(name))
            .cloned()
        {
            return self.mod_subst(&l, argv);
        }
        if self.ns_has(name, false) {
            return self.ns_subst(argv);
        }
        Err(Fail::plain(&format!("{name}: unknown ${{}} builtin")))
    }

    /// Runs one `${}` builtin of a loaded module.
    pub(crate) fn mod_subst(&mut self, l: &Loaded, argv: &[String]) -> Result<Vec<String>, Fail> {
        let m = match l {
            Loaded::Builtin(m) => *m,
            Loaded::Reactor(name, handle) => {
                let args = argv[1..].join(" ");
                let out = purgatorio::call(*handle, name, args.as_bytes())
                    .map_err(|e| Fail::plain(&format!("{name}: {e}")))?;
                let text = String::from_utf8_lossy(&out);
                return Ok(fields(&text));
            }
        };
        match m {
            Module::Std => Ok(std_subst(argv)?),
            Module::Expr => crate::num::expr(argv, false),
            Module::Mpexpr => crate::num::expr(argv, true),
            Module::String => Ok(crate::string::subst(argv)?),
            Module::Regex => Ok(crate::regex::re(argv)?),
            Module::Csv => Ok(vec![crate::csv::join(&argv[1..])]),
            Module::File2chan => self.rget(argv),
            Module::Sexprs => crate::sexp::subst(argv),
            Module::Arg | Module::Echo | Module::Mload | Module::Test => {
                Err(Fail::plain(&format!("{}: unknown ${{}} builtin", argv[0])))
            }
        }
    }

    pub(crate) fn std_cmd(&mut self, argv: &[String]) -> Flow<String> {
        match argv[0].as_str() {
            "if" => self.std_if(argv),
            "while" => self.std_while(argv),
            "for" => self.std_for(argv),
            "and" => self.std_and(argv),
            "or" => self.std_or(argv),
            "not" => self.std_not(argv),
            "~" => self.std_tilde(argv),
            "fn" => self.std_fn(argv),
            "raise" => Err(Stop::Raise(if argv.len() == 1 {
                String::from("raise")
            } else {
                argv[1..].join(" ")
            })),
            _ => Ok(String::from("fail:unknown builtin")),
        }
    }

    fn std_if(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() < 3 || argv.len() > 4 {
            self.status = String::from("usage: if cond then [else]");
            return Ok(self.status.clone());
        }
        self.run_block(&argv[1])?;
        if self.status.is_empty() {
            self.run_block(&argv[2])?;
        } else if argv.len() == 4 {
            self.run_block(&argv[3])?;
        } else {
            self.status.clear();
        }
        Ok(self.status.clone())
    }

    fn std_while(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() != 3 {
            self.status = String::from("usage: while cond body");
            return Ok(self.status.clone());
        }
        loop {
            self.run_block(&argv[1])?;
            if !self.status.is_empty() {
                self.status.clear();
                break;
            }
            self.run_block(&argv[2])?;
        }
        Ok(self.status.clone())
    }

    fn std_for(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() < 4 || argv[2] != "in" || !argv.last().is_some_and(|s| s.starts_with('{')) {
            self.status = String::from("usage: for var in list body");
            return Ok(self.status.clone());
        }
        let var = argv[1].clone();
        let body = argv.last().unwrap().clone();
        let values: Vec<String> = argv[3..argv.len() - 1].to_vec();
        for value in values {
            self.set_var(&var, vec![value], false);
            self.run_block(&body)?;
        }
        Ok(self.status.clone())
    }

    fn std_and(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() < 3 {
            self.status = String::from("usage: and cmd ...");
            return Ok(self.status.clone());
        }
        for cmd in &argv[1..] {
            self.run_block(cmd)?;
            if !self.status.is_empty() {
                break;
            }
        }
        Ok(self.status.clone())
    }

    fn std_or(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() < 3 {
            self.status = String::from("usage: or cmd ...");
            return Ok(self.status.clone());
        }
        for cmd in &argv[1..] {
            self.run_block(cmd)?;
            if self.status.is_empty() {
                break;
            }
        }
        Ok(self.status.clone())
    }

    fn std_not(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() != 2 {
            self.status = String::from("usage: not cmd");
            return Ok(self.status.clone());
        }
        self.run_block(&argv[1])?;
        self.status = if self.status.is_empty() {
            String::from("false")
        } else {
            String::new()
        };
        Ok(self.status.clone())
    }

    fn std_tilde(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() < 3 {
            self.status = String::from("usage: ~ word pattern ...");
            return Ok(self.status.clone());
        }
        let ok = argv[2..].iter().any(|pat| glob_match(&argv[1], pat));
        self.status = if ok {
            String::new()
        } else {
            String::from("no match")
        };
        Ok(self.status.clone())
    }

    fn std_fn(&mut self, argv: &[String]) -> Flow<String> {
        if argv.len() != 3 || !argv[2].starts_with('{') {
            self.status = String::from("usage: fn name body");
            return Ok(self.status.clone());
        }
        self.fns.insert(argv[1].clone(), argv[2].clone());
        self.status.clear();
        Ok(String::new())
    }

    fn call_fn(&mut self, body: &str, argv: &[String]) -> Flow<String> {
        let saved = self.vars.get("*").cloned();
        self.set_var("*", argv[1..].to_vec(), false);
        let result = self.run_block(body);
        match saved {
            Some(v) => {
                self.vars.insert(String::from("*"), v);
            }
            None => {
                self.vars.remove("*");
            }
        }
        result?;
        Ok(self.status.clone())
    }

    pub(crate) fn run_block(&mut self, src: &str) -> Flow<()> {
        let inner = src.trim();
        let inner = inner
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(inner);
        self.run_src(inner)
    }
}

/// A word being expanded: its text, and the byte offsets of the `*`, `?`
/// and `[` that were written unquoted. Only those make a file name
/// pattern; the same characters from quotes, variables and substitutions
/// stand for themselves.
#[derive(Debug, Clone, PartialEq)]
struct Glob {
    text: String,
    meta: Vec<usize>,
}

impl Glob {
    fn lit(s: &str, quoted: bool) -> Glob {
        let meta = match quoted {
            true => Vec::new(),
            false => s.match_indices(['*', '?', '[']).map(|m| m.0).collect(),
        };
        Glob {
            text: String::from(s),
            meta,
        }
    }

    fn plain(words: Vec<String>) -> Vec<Glob> {
        let plain = |text| Glob {
            text,
            meta: Vec::new(),
        };
        words.into_iter().map(plain).collect()
    }

    fn join(mut self, other: &Glob) -> Glob {
        let base = self.text.len();
        self.meta.extend(other.meta.iter().map(|m| base + m));
        self.text.push_str(&other.text);
        self
    }

    /// The file names the word matches, sorted, or the word itself if it
    /// has no pattern or nothing matches.
    fn names(self) -> Vec<String> {
        if self.meta.is_empty() {
            return vec![self.text];
        }
        let marks: Vec<filepat::Mark> = self
            .text
            .char_indices()
            .map(|(i, c)| (c, self.meta.contains(&i)))
            .collect();
        filepat::expand_marked(&marks)
    }
}

/// Joins two lists: a one-element list distributes over the other, and
/// equal-length lists join pairwise.
fn concat(a: Vec<Glob>, b: Vec<Glob>) -> Result<Vec<Glob>, Error> {
    if a.is_empty() || b.is_empty() {
        return Err(Error::from("null list in concatenation"));
    }
    Ok(match (a.len(), b.len()) {
        (1, _) => b.iter().map(|y| a[0].clone().join(y)).collect(),
        (_, 1) => a.into_iter().map(|x| x.join(&b[0])).collect(),
        (n, m) if n == m => a.into_iter().zip(&b).map(|(x, y)| x.join(y)).collect(),
        _ => return Err(Error::from("bad concatenation")),
    })
}

/// The list builtins of `load std`: `${hd list}`, `${tl list}`,
/// `${index n list}`, `${quote list}` and `${unquote arg}`, and `${pid}`,
/// this prog's pid.
fn std_subst(argv: &[String]) -> Result<Vec<String>, Error> {
    let list = &argv[1..];
    Ok(match argv[0].as_str() {
        "pid" => vec![purgatorio::pid().to_string()],
        "hd" => list.first().cloned().into_iter().collect(),
        "tl" => list.iter().skip(1).cloned().collect(),
        "index" => {
            let n = list
                .first()
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(|| Error::from("usage: index number list"))?;
            n.checked_sub(1)
                .and_then(|i| list[1..].get(i))
                .cloned()
                .into_iter()
                .collect()
        }
        "quote" => {
            let q: Vec<String> = list.iter().map(|w| quote(w)).collect();
            vec![q.join(" ")]
        }
        _ => {
            let [arg] = list else {
                return Err(Error::from("usage: unquote arg"));
            };
            unquote(arg)
        }
    })
}

/// The words of a command's output: it is split at blanks only, so a word
/// may hold other white space such as U+00A0.
fn fields(s: &str) -> Vec<String> {
    s.split(is_blank)
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}

/// Splits `s` into words at blanks, honouring `'quotes'` as `quote`
/// writes them.
fn unquote(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut c = s.chars().peekable();
    loop {
        while c.next_if(|&c| is_blank(c)).is_some() {}
        if c.peek().is_none() {
            return words;
        }
        let mut w = String::new();
        while let Some(ch) = c.next_if(|&c| !is_blank(c)) {
            if ch != '\'' {
                w.push(ch);
                continue;
            }
            loop {
                match c.next() {
                    None => break,
                    Some('\'') if c.next_if_eq(&'\'').is_some() => w.push('\''),
                    Some('\'') => break,
                    Some(ch) => w.push(ch),
                }
            }
        }
        words.push(w);
    }
}

/// The command name a word gives before expansion: its text if it is
/// plain, or the first word of a list such as `(arg ...)`.
fn head_name(w: &Word) -> Option<&str> {
    match w.parts.as_slice() {
        [Part::List(words)] => words.first().and_then(head_name),
        _ => w.bare(),
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(name, "cd" | "exit" | "run" | "wait" | "load" | "@")
}

/// `~`: whether the whole of `word` matches `pat`. The pattern has the
/// syntax of a file name pattern, but `/` is an ordinary character.
fn glob_match(word: &str, pat: &str) -> bool {
    filepat::matches_marked(&filepat::marks(pat), word)
}

/// The module a command name runs ([`purgatorio::find_command`]): `ls` and
/// `ndb/query` are under `/bin`, a name starting with `/`, `./`, `../` or
/// `#` is a path.
fn resolve(name: &str) -> Result<String, Error> {
    purgatorio::find_command(name)
        .map_err(|_| Error(format!("{name}: {}", purgatorio::abi::error::ENONEXIST)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| String::from(*s)).collect()
    }

    fn cat(a: &[&str], b: &[&str]) -> Result<Vec<String>, Error> {
        let words = concat(Glob::plain(v(a)), Glob::plain(v(b)))?;
        Ok(words.into_iter().map(|g| g.text).collect())
    }

    #[test]
    fn concatenation() {
        assert_eq!(cat(&["a"], &["1", "2"]).unwrap(), ["a1", "a2"]);
        assert_eq!(cat(&["x", "y"], &[".c"]).unwrap(), ["x.c", "y.c"]);
        assert_eq!(cat(&["x", "y"], &["1", "2"]).unwrap(), ["x1", "y2"]);
        assert!(cat(&["x", "y"], &["1", "2", "3"]).is_err());
        assert!(cat(&[], &["1"]).is_err());
    }

    #[test]
    fn only_unquoted_characters_make_a_pattern() {
        assert!(Glob::lit("a*[b]?", true).meta.is_empty());
        assert_eq!(Glob::lit("a*[b]?", false).meta, [1, 2, 5]);
        // 'é*'^x*^$y: the quoted star stays literal and offsets are bytes.
        let quoted = Glob::lit("é*", true);
        let word = quoted.join(&Glob::lit("x*", false));
        let word = word.join(&Glob::plain(v(&["?"]))[0]);
        assert_eq!(word.text, "é*x*?");
        assert_eq!(word.meta, [4]);
        assert_eq!(Glob::lit("no pattern", false).names(), ["no pattern"]);
    }

    #[test]
    fn patterns() {
        assert!(glob_match("hello", "h*o"));
        assert!(glob_match("yes", "yes"));
        assert!(!glob_match("ab", "a"));
        assert!(glob_match("a/b.c", "*.[ch]"));
        assert!(glob_match("abc", "a?c"));
        assert!(glob_match("abc", "*b*"));
        assert!(!glob_match("abc", "[^a]*"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's here"), "'it''s here'");
        assert_eq!(unquote("a 'b c' '' 'it''s'x"), ["a", "b c", "", "it'sx"]);
        assert_eq!(quote("a\u{a0}b"), "a\u{a0}b");
        assert_eq!(unquote("a\u{a0}b\tc"), ["a\u{a0}b", "c"]);
        assert_eq!(fields(" a\u{a0}b\r\n\tc\n"), ["a\u{a0}b", "c"]);
        assert_eq!(
            std_subst(&v(&["index", "2", "a", "b", "c"])).unwrap(),
            ["b"]
        );
        assert!(std_subst(&v(&["index", "4", "a"])).unwrap().is_empty());
    }
}
