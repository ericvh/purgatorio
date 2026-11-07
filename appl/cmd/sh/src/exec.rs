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

use crate::lex::{Part, RedirKind, VarKind, Word};
use crate::parse::{self, AndOr, Cmd, CmdKind, Item, Pipeline, Redir};

pub const SH: &str = "/bin/sh.wasm";

/// Variables that are never written to `/env`.
fn exported(name: &str) -> bool {
    !(name == "*" || name == "status" || name == "apid" || name.chars().all(|c| c.is_ascii_digit()))
}

/// The fds a command sees, as `(its fd, this prog's fd)`.
#[derive(Clone)]
struct Io {
    map: Vec<(i32, i32)>,
}

impl Io {
    fn std() -> Io {
        Io {
            map: vec![(0, 0), (1, 1), (2, 2)],
        }
    }

    fn get(&self, fd: i32) -> Option<i32> {
        self.map.iter().find(|m| m.0 == fd).map(|m| m.1)
    }

    fn set(&mut self, fd: i32, to: i32) {
        self.map.retain(|m| m.0 != fd);
        self.map.push((fd, to));
    }

    fn fdmap(&self) -> Vec<(i32, i32)> {
        self.map
            .iter()
            .map(|&(child, ours)| (ours, child))
            .collect()
    }

    fn write(&self, fd: i32, s: &str) {
        if let Some(fd) = self.get(fd) {
            // SAFETY: `fd` is open in this prog for the duration of the
            // command, and ManuallyDrop keeps the File from closing it.
            let f = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
            let _ = (&*f).write_all(s.as_bytes());
        }
    }
}

/// Quotes a word for a sub-shell or `/env` when it needs it.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && !s
            .chars()
            .any(|c| c.is_whitespace() || "'#;&|{}()<>$`^=".contains(c))
    {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

pub struct Shell {
    vars: HashMap<String, Vec<String>>,
    locals: HashSet<String>,
    /// Background progs not yet seen to exit.
    background: HashSet<u32>,
    /// Exit messages for progs not yet waited for.
    exited: HashMap<u32, String>,
    pub status: String,
}

/// Why execution stopped early.
pub enum Stop {
    Exit(String),
}

type Flow<T> = Result<T, Stop>;

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
        }
    }

    fn set_var(&mut self, name: &str, value: Vec<String>, local: bool) {
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

    fn var(&self, name: &str) -> Vec<String> {
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
        for item in items {
            if item.background {
                self.background_item(item, io);
            } else {
                self.status = self.andor(&item.andor, io)?;
            }
        }
        Ok(())
    }

    fn andor(&mut self, a: &AndOr, io: &Io) -> Flow<String> {
        let mut status = self.pipeline(&a.first, io)?;
        for (and, p) in &a.rest {
            if status.is_empty() == *and {
                self.status = status;
                status = self.pipeline(p, io)?;
            }
        }
        Ok(status)
    }

    fn pipeline(&mut self, p: &Pipeline, io: &Io) -> Flow<String> {
        let status = if let [cmd] = p.cmds.as_slice()
            && runs_here(cmd)
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
                Err(e) => {
                    eprintln!("sh: {e}");
                    format!("fail:{e}")
                }
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
        let started =
            if a.rest.is_empty() && !a.first.bang && a.first.cmds.iter().all(|c| !runs_here(c)) {
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
            Err(e) => self.fail(&e.0),
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
            let started = if runs_here(cmd) {
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
        let argv = vec![String::from("sh"), String::from("-c"), String::from(src)];
        purgatorio::spawn(SH, &argv, pctl::NEWFD, &io.fdmap())
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

    /// Waits until every prog in `pids` has exited and returns their
    /// statuses, in order. Messages from other children are kept.
    fn wait_for(&mut self, pids: &[u32]) -> Vec<String> {
        while !pids.iter().all(|p| self.exited.contains_key(p)) {
            match purgatorio::wait() {
                Ok(Waitmsg { pid, status, .. }) => {
                    if !self.background.remove(&pid) {
                        self.exited.insert(pid, status);
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

    /// Runs a builtin, assignment or block in this prog.
    fn here(&mut self, cmd: &Cmd, io: &Io) -> Flow<String> {
        let mut io = io.clone();
        let _files = match self.redirect(&cmd.redirs, &mut io) {
            Ok(f) => f,
            Err(e) => {
                io.write(2, &format!("sh: {e}\n"));
                return Ok(format!("fail:{e}"));
            }
        };
        match &cmd.kind {
            CmdKind::Assign { name, local, value } => match self.expand_all(value) {
                Ok(v) => {
                    self.set_var(name, v, *local);
                    Ok(String::new())
                }
                Err(e) => {
                    io.write(2, &format!("sh: {e}\n"));
                    Ok(format!("fail:{e}"))
                }
            },
            CmdKind::Block(items) => {
                self.run_items(items, &io)?;
                Ok(self.status.clone())
            }
            CmdKind::Simple(words) => {
                let argv = match self.expand_all(words) {
                    Ok(a) => a,
                    Err(e) => {
                        io.write(2, &format!("sh: {e}\n"));
                        return Ok(format!("fail:{e}"));
                    }
                };
                self.builtin(&argv, &io)
            }
        }
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
            "load" => Err(Error::from(
                "load: loadable builtins are not implemented yet",
            )),
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

    fn expand(&mut self, w: &Word) -> Result<Vec<String>, Error> {
        let mut acc: Option<Vec<String>> = None;
        for p in &w.parts {
            let v = match p {
                Part::Lit(s, _) => vec![s.clone()],
                Part::Var(name, kind) => {
                    let v = self.var(name);
                    match kind {
                        VarKind::Plain => v,
                        VarKind::Count => vec![v.len().to_string()],
                        VarKind::Join => vec![v.join(" ")],
                    }
                }
                Part::Subst(src) => self.substitute(src)?,
                Part::List(words) => self.expand_all(words)?,
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
        Ok(out.split_whitespace().map(String::from).collect())
    }
}

/// Joins two lists: a one-element list distributes over the other, and
/// equal-length lists join pairwise.
fn concat(a: Vec<String>, b: Vec<String>) -> Result<Vec<String>, Error> {
    if a.is_empty() || b.is_empty() {
        return Err(Error::from("null list in concatenation"));
    }
    Ok(match (a.len(), b.len()) {
        (1, _) => b.into_iter().map(|y| format!("{}{y}", a[0])).collect(),
        (_, 1) => a.into_iter().map(|x| format!("{x}{}", b[0])).collect(),
        (n, m) if n == m => a.into_iter().zip(b).map(|(x, y)| x + &y).collect(),
        _ => return Err(Error::from("bad concatenation")),
    })
}

fn is_builtin(name: &str) -> bool {
    matches!(name, "cd" | "exit" | "run" | "wait" | "load")
}

/// Whether a command runs inside the shell rather than as a new prog.
fn runs_here(cmd: &Cmd) -> bool {
    match &cmd.kind {
        CmdKind::Simple(words) => words.first().and_then(Word::bare).is_some_and(is_builtin),
        CmdKind::Assign { .. } | CmdKind::Block(_) => true,
    }
}

/// The module a command name runs: `/bin/name.wasm` for a plain name; a
/// name containing `/` is used as given, or with `.wasm` added.
fn resolve(name: &str) -> Result<String, Error> {
    let candidates = if name.contains('/') {
        vec![String::from(name), format!("{name}.wasm")]
    } else {
        vec![format!("/bin/{name}.wasm")]
    };
    for c in &candidates {
        if purgatorio::stat(c).is_ok_and(|d| !d.qid.is_dir()) {
            return Ok(c.clone());
        }
    }
    Err(Error(format!(
        "{name}: {}",
        purgatorio::abi::error::ENONEXIST
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn concatenation() {
        assert_eq!(concat(v(&["a"]), v(&["1", "2"])).unwrap(), ["a1", "a2"]);
        assert_eq!(concat(v(&["x", "y"]), v(&[".c"])).unwrap(), ["x.c", "y.c"]);
        assert_eq!(
            concat(v(&["x", "y"]), v(&["1", "2"])).unwrap(),
            ["x1", "y2"]
        );
        assert!(concat(v(&["x", "y"]), v(&["1", "2", "3"])).is_err());
        assert!(concat(v(&[]), v(&["1"])).is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's here"), "'it''s here'");
    }
}
