//! `#C`, the command device, as Inferno's cmd(3).
//!
//! Opening `clone` makes a conversation `n` and gives its `ctl`. `ctl`
//! takes `dir wdir`, `nice [n]`, `exec command args...` (run directly on
//! the host, with the arguments as written, quoted as parsecmd quotes),
//! `kill` and `killonclose`; reading it gives `n`. `data` opened for
//! reading gives the command's standard output, and opened for writing
//! feeds its standard input, which the last such close ends. `stderr`
//! gives its error output, `wait` blocks until it exits and then gives
//! `pid user sys real 'status'`, and `status` gives
//! `cmd/n opens state wdir arg0`. A running command is killed when every
//! `ctl`, `data`, `stderr` and `wait` file of its conversation is closed,
//! or when `ctl` is closed after `killonclose`; a conversation with no
//! files open and nothing running goes away. Platforms with no host
//! commands answer `exec` with `"not implemented"`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use purg_abi::{error, mode};
use purg_platform::net::{CmdSpec, Command};
use purg_platform::sync::Mutex;
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

use crate::Platform;
use crate::chan::Chan;
use crate::dev::{Dev, Walked, read_str};
use crate::error::{Error, Result};

const QROOT: u64 = 0;
const QCLONE: u64 = 1;
/// Above the small fixed qids. The id occupies bits 8..40, so this bit must
/// sit above them.
const BASE: u64 = 1 << 40;

const FDIR: u8 = 0;
const FCTL: u8 = 1;
const FDATA: u8 = 2;
const FSTDERR: u8 = 3;
const FWAIT: u8 = 4;
const FSTATUS: u8 = 5;

/// Inferno's error for a control message with the wrong number of fields.
const ECMDARGS: &str = "wrong #args in control message";
/// Inferno's `Etoosmall`.
const ETOOSMALL: &str = "read or write too small";

fn qfile(id: u32, file: u8) -> u64 {
    BASE | (u64::from(id) << 8) | u64::from(file)
}

fn id_of(path: u64) -> u32 {
    ((path >> 8) & 0xffff_ffff) as u32
}

fn file_of(path: u64) -> u8 {
    (path & 0xff) as u8
}

/// A conversation's settings and its command.
#[derive(Default)]
struct State {
    cmd: Option<Arc<dyn Command>>,
    /// `dir`: where the command runs; `None` is the platform's default.
    dir: Option<String>,
    /// `nice`: 0 for normal priority, 1 to 3 lower.
    nice: u8,
    killonclose: bool,
    killed: bool,
    /// The command's name, for `status`.
    arg0: String,
    /// Open `ctl`, `data`, `stderr` and `wait` files.
    inuse: usize,
    /// Of those, `data` files open for writing.
    writers: usize,
    /// When the command started, in monotonic ns.
    started: u64,
    /// `wait`'s line, once the command has exited.
    waited: Option<String>,
}

struct Conv {
    id: u32,
    st: Mutex<State>,
}

impl Conv {
    fn cmd(&self) -> Option<Arc<dyn Command>> {
        self.st.lock().cmd.clone()
    }
}

/// Splits a control message into fields as Inferno's parsecmd does: at
/// white space, except inside single quotes, where a doubled quote is
/// one quote.
fn fields(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            return out;
        }
        let mut f = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if !quoted && c.is_whitespace() {
                break;
            }
            chars.next();
            if c != '\'' {
                f.push(c);
            } else if quoted && chars.peek() == Some(&'\'') {
                chars.next();
                f.push('\'');
            } else {
                quoted = !quoted;
            }
        }
        out.push(f);
    }
}

/// A string as Inferno's `%q` prints it: quoted with single quotes, a
/// quote doubled, when it is empty or holds white space or a quote.
fn quote(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// Inferno's `cmderror`: the message, then the fields in quotes.
fn cmd_error(what: &str, f: &[String]) -> Error {
    let words: Vec<String> = f.iter().map(|w| quote(w)).collect();
    Error::from(format!("{what} \"{}\"", words.join(" ")))
}

/// `#C`.
pub struct DevCmd {
    platform: Arc<dyn Platform>,
    convs: Mutex<Vec<Arc<Conv>>>,
}

impl DevCmd {
    pub fn new(platform: Arc<dyn Platform>) -> DevCmd {
        DevCmd {
            platform,
            convs: Mutex::new(Vec::new()),
        }
    }

    /// A new conversation, numbered with the lowest number not in use,
    /// as Inferno's `clone` reuses closed ones.
    fn alloc(&self) -> Arc<Conv> {
        let mut convs = self.convs.lock();
        let id = (0..)
            .find(|n| !convs.iter().any(|c| c.id == *n))
            .unwrap_or(0);
        let c = Arc::new(Conv {
            id,
            st: Mutex::new(State::default()),
        });
        convs.push(Arc::clone(&c));
        c
    }

    fn find(&self, id: u32) -> Option<Arc<Conv>> {
        self.convs.lock().iter().find(|c| c.id == id).cloned()
    }

    fn conv(&self, c: &Chan) -> Result<Arc<Conv>> {
        self.find(id_of(c.qid.path))
            .ok_or(Error::new(error::ENONEXIST))
    }

    fn qid(path: u64) -> Qid {
        let qtype = if path == QROOT || (path >= BASE && file_of(path) == FDIR) {
            QTDIR
        } else {
            QTFILE
        };
        Qid {
            path,
            vers: 0,
            qtype,
        }
    }

    fn dirent(&self, path: u64) -> Dir {
        let (name, _qtype, perm) = match path {
            QROOT => (String::from("#C"), QTDIR, DMDIR | 0o555),
            QCLONE => (String::from("clone"), QTFILE, 0o666),
            q if q >= BASE => match file_of(q) {
                FDIR => (id_of(q).to_string(), QTDIR, DMDIR | 0o555),
                FCTL => (String::from("ctl"), QTFILE, 0o666),
                FDATA => (String::from("data"), QTFILE, 0o666),
                FSTDERR => (String::from("stderr"), QTFILE, 0o444),
                FWAIT => (String::from("wait"), QTFILE, 0o444),
                FSTATUS => (String::from("status"), QTFILE, 0o444),
                _ => (String::from("?"), QTFILE, 0o444),
            },
            _ => (String::from("?"), QTFILE, 0o444),
        };
        let user = self.platform.user();
        Dir {
            dtype: u16::from(b'C'),
            qid: Self::qid(path),
            mode: perm,
            name,
            uid: user.clone(),
            gid: user,
            ..Dir::default()
        }
    }

    /// The `status` line: `cmd/n opens state wdir arg0`.
    fn status(conv: &Conv) -> String {
        let st = conv.st.lock();
        let state = match &st.cmd {
            None => "Open",
            Some(_) if st.waited.is_some() => "Done",
            Some(cmd) if cmd.status() == "Running" => "Execute",
            Some(_) => "Done",
        };
        format!(
            "cmd/{} {} {state} {} {}\n",
            conv.id,
            st.inuse,
            quote(st.dir.as_deref().unwrap_or("")),
            quote(&st.arg0)
        )
    }

    /// A message written to `ctl`.
    fn ctl(&self, conv: &Conv, text: &str) -> Result<()> {
        let f = fields(text);
        let Some(verb) = f.first() else {
            return Err(Error::new("empty control message"));
        };
        match verb.as_str() {
            "dir" if f.len() == 2 => conv.st.lock().dir = Some(f[1].clone()),
            "nice" if f.len() <= 2 => {
                let n = f.get(1).map_or(1, |n| n.parse::<u8>().unwrap_or(0));
                conv.st.lock().nice = n.min(3);
            }
            "killonclose" if f.len() == 1 => conv.st.lock().killonclose = true,
            "kill" if f.len() == 1 => {
                let cmd = conv.cmd().ok_or(Error::new("not started"))?;
                cmd.kill().map_err(Error::from)?;
                conv.st.lock().killed = true;
            }
            "exec" => {
                if f.len() < 2 {
                    return Err(Error::new(ETOOSMALL));
                }
                let (dir, nice) = {
                    let st = conv.st.lock();
                    if st.cmd.is_some() {
                        return Err(Error::new(error::EINUSE));
                    }
                    (st.dir.clone(), st.nice)
                };
                let spec = CmdSpec {
                    argv: &f[1..],
                    dir: dir.as_deref(),
                    nice,
                };
                let cmd = self.platform.command(&spec).map_err(Error::from)?;
                let mut st = conv.st.lock();
                st.arg0 = f[1].clone();
                st.started = self.platform.monotonic_ns();
                st.cmd = Some(cmd);
            }
            "dir" | "nice" | "killonclose" | "kill" => return Err(cmd_error(ECMDARGS, &f)),
            _ => return Err(cmd_error("unknown control message", &f)),
        }
        Ok(())
    }

    /// `wait`'s line, once the command has exited: host pid, user and
    /// system time (unknown here, so 0), real time in ms, and the exit
    /// status quoted.
    fn wait_line(&self, conv: &Conv) -> Result<String> {
        if let Some(line) = conv.st.lock().waited.clone() {
            return Ok(line);
        }
        let cmd = conv.cmd().ok_or(Error::new("not started"))?;
        let status = cmd.wait().map_err(Error::from)?;
        let mut st = conv.st.lock();
        let ms = self.platform.monotonic_ns().saturating_sub(st.started) / 1_000_000;
        let line = format!("{} 0 0 {ms} {}\n", cmd.pid(), quote(&status));
        st.waited = Some(line.clone());
        Ok(line)
    }
}

impl Dev for DevCmd {
    fn dc(&self) -> char {
        'C'
    }

    fn name(&self) -> &'static str {
        "cmd"
    }

    fn attach(&self, _spec: &str) -> Result<Walked> {
        Ok(Walked {
            qid: Self::qid(QROOT),
            aux: None,
        })
    }

    fn walk(&self, c: &Chan, name: &str) -> Result<Walked> {
        let next = match c.qid.path {
            QROOT if name == "clone" => QCLONE,
            QROOT => {
                let id: u32 = name.parse().map_err(|_| Error::new(error::ENONEXIST))?;
                let conv = self.find(id).ok_or(Error::new(error::ENONEXIST))?;
                qfile(conv.id, FDIR)
            }
            q if q >= BASE && file_of(q) == FDIR => match name {
                "ctl" => qfile(id_of(q), FCTL),
                "data" => qfile(id_of(q), FDATA),
                "stderr" => qfile(id_of(q), FSTDERR),
                "wait" => qfile(id_of(q), FWAIT),
                "status" => qfile(id_of(q), FSTATUS),
                _ => return Err(Error::new(error::ENONEXIST)),
            },
            _ => return Err(Error::new(error::ENOTDIR)),
        };
        Ok(Walked {
            qid: Self::qid(next),
            aux: None,
        })
    }

    fn stat(&self, c: &Chan) -> Result<Dir> {
        Ok(self.dirent(c.qid.path))
    }

    fn open(&self, c: &mut Chan, omode: u8) -> Result<()> {
        let access = omode & mode::ACCESS;
        if c.qid.qtype == QTDIR && access != mode::OREAD {
            return Err(Error::new(error::EISDIR));
        }
        if c.qid.path == QCLONE {
            let conv = self.alloc();
            c.qid = Self::qid(qfile(conv.id, FCTL));
        }
        if c.qid.path < BASE {
            return Ok(());
        }
        let file = file_of(c.qid.path);
        if matches!(file, FSTDERR | FWAIT | FSTATUS) && access != mode::OREAD {
            return Err(Error::new(error::EPERM));
        }
        if matches!(file, FCTL | FDATA | FSTDERR | FWAIT) {
            let conv = self.conv(c)?;
            let mut st = conv.st.lock();
            st.inuse += 1;
            if file == FDATA && matches!(access, mode::OWRITE | mode::ORDWR) {
                st.writers += 1;
            }
        }
        Ok(())
    }

    /// The last `data` opened for writing ends the command's standard
    /// input. The last of the conversation's files kills a command still
    /// running, as does `ctl` after `killonclose`; with nothing running,
    /// the conversation goes.
    fn close(&self, c: &Chan) {
        if !c.open || c.qid.path < BASE {
            return;
        }
        let file = file_of(c.qid.path);
        if !matches!(file, FCTL | FDATA | FSTDERR | FWAIT) {
            return;
        }
        let Ok(conv) = self.conv(c) else {
            return;
        };
        let access = c.mode & mode::ACCESS;
        let (close_stdin, kill, cmd, gone) = {
            let mut st = conv.st.lock();
            let mut close_stdin = false;
            if file == FDATA && matches!(access, mode::OWRITE | mode::ORDWR) {
                st.writers = st.writers.saturating_sub(1);
                close_stdin = st.writers == 0;
            }
            st.inuse = st.inuse.saturating_sub(1);
            let running = st.cmd.is_some() && st.waited.is_none();
            let kill = running && !st.killed && (st.inuse == 0 || (st.killonclose && file == FCTL));
            if kill {
                st.killed = true;
            }
            let gone = st.inuse == 0 && (st.cmd.is_none() || !running || kill);
            (close_stdin, kill, st.cmd.clone(), gone)
        };
        if let Some(cmd) = cmd {
            if close_stdin {
                cmd.close_stdin();
            }
            if kill {
                let _ = cmd.kill();
            }
        }
        if gone {
            self.convs.lock().retain(|v| v.id != conv.id);
        }
    }

    fn read(&self, c: &Chan, buf: &mut [u8], offset: u64) -> Result<usize> {
        if c.qid.path < BASE {
            return Err(Error::new(error::EISDIR));
        }
        let conv = self.conv(c)?;
        match file_of(c.qid.path) {
            FCTL => Ok(read_str(format!("{}\n", conv.id).as_bytes(), buf, offset)),
            // Before `exec` there is nothing to read: end of file.
            FDATA => match conv.cmd() {
                Some(cmd) => cmd.read_stdout(buf).map_err(Error::from),
                None => Ok(0),
            },
            FSTDERR => match conv.cmd() {
                Some(cmd) => cmd.read_stderr(buf).map_err(Error::from),
                None => Ok(0),
            },
            FWAIT => {
                let line = self.wait_line(&conv)?;
                Ok(read_str(line.as_bytes(), buf, offset))
            }
            FSTATUS => Ok(read_str(Self::status(&conv).as_bytes(), buf, offset)),
            _ => Err(Error::new(error::EISDIR)),
        }
    }

    fn dirread(&self, c: &Chan) -> Result<Vec<Dir>> {
        let mut entries = Vec::new();
        match c.qid.path {
            QROOT => {
                entries.push(self.dirent(QCLONE));
                for conv in self.convs.lock().iter() {
                    entries.push(self.dirent(qfile(conv.id, FDIR)));
                }
            }
            q if q >= BASE && file_of(q) == FDIR => {
                for f in [FCTL, FDATA, FSTDERR, FWAIT, FSTATUS] {
                    entries.push(self.dirent(qfile(id_of(q), f)));
                }
            }
            _ => return Err(Error::new(error::ENOTDIR)),
        }
        Ok(entries)
    }

    fn write(&self, c: &Chan, buf: &[u8], _offset: u64) -> Result<usize> {
        if c.qid.path < BASE {
            return Err(Error::new(error::EPERM));
        }
        let conv = self.conv(c)?;
        match file_of(c.qid.path) {
            FCTL => {
                let text = core::str::from_utf8(buf).map_err(|_| Error::new(error::EBADARG))?;
                self.ctl(&conv, text)?;
                Ok(buf.len())
            }
            // Before `exec`, or once standard input is closed, there is
            // nowhere for the data to go.
            FDATA => {
                let cmd = conv.cmd().ok_or(Error::new(error::EHUNGUP))?;
                let mut done = 0;
                while done < buf.len() {
                    match cmd.write_stdin(&buf[done..]) {
                        Ok(0) | Err(_) if done == 0 => return Err(Error::new(error::EHUNGUP)),
                        Ok(0) | Err(_) => break,
                        Ok(n) => done += n,
                    }
                }
                Ok(done)
            }
            _ => Err(Error::new(error::EPERM)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_follow_parsecmd() {
        assert_eq!(fields("exec echo hello\n"), ["exec", "echo", "hello"]);
        assert_eq!(
            fields("exec sh -c 'a b' ''"),
            ["exec", "sh", "-c", "a b", ""]
        );
        assert_eq!(fields("exec echo 'it''s'"), ["exec", "echo", "it's"]);
        assert!(fields("  \n").is_empty());
    }

    #[test]
    fn quote_follows_percent_q() {
        assert_eq!(quote("ls"), "ls");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("exit 1"), "'exit 1'");
        assert_eq!(quote("it's"), "'it''s'");
    }
}
