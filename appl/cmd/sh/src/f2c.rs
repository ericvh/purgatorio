//! `load file2chan`: a file served by shell commands, after
//! sh-file2chan(1).
//!
//! Inferno serves the file from a new thread of the shell. A wasm prog has
//! one thread, so `file2chan` starts a sub-shell, `sh -F`, that creates
//! the file with the kernel's file2chan and runs the commands for each
//! request. The sub-shell gets the shell's modules and functions, and its
//! variables through `/env`; like Inferno's thread, it keeps its own
//! variables from then on. The kernel does not tell a file2chan server
//! when a client closes the file, so a close command is accepted but never
//! run.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use purgatorio::Error;
use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, Request};

use crate::exec::{Io, SH, Shell, Stop};
use crate::shmods::{Ex, Fail, is_block};

/// The fd on which the server says it is ready.
/// (3 and 4 are the WASI preopens.)
const READY_FD: i32 = 5;

const USAGE: &str = "usage: file2chan file {readcmd} {writecmd} [ {closecmd} ]";

/// An outstanding request.
pub struct Tag {
    req: Request,
    /// `rblock`: no automatic reply.
    blocked: bool,
}

#[derive(Default)]
pub struct Tags {
    next: u64,
    tags: HashMap<u64, Tag>,
}

/// Splits a path into the directory and the name of the file to make.
fn split(path: &str) -> (String, String) {
    let path = path.trim_end_matches('/');
    let path = if path.starts_with('/') {
        String::from(path)
    } else {
        let dot = purgatorio::dot().unwrap_or_else(|_| String::from("/"));
        format!("{}/{path}", dot.trim_end_matches('/'))
    };
    match path.rsplit_once('/') {
        Some(("", name)) => (String::from("/"), String::from(name)),
        Some((dir, name)) => (String::from(dir), String::from(name)),
        None => (String::from("/"), path),
    }
}

impl Shell {
    /// `file2chan file {readcmd} {writecmd} [{closecmd}]`.
    pub(crate) fn file2chan(&mut self, argv: &[String], io: &Io) -> Result<String, Ex> {
        if !(4..=5).contains(&argv.len())
            || argv[1].is_empty()
            || !argv[2..].iter().all(|c| is_block(c))
        {
            return Err(Fail::raise("usage", USAGE).into());
        }
        let started = (|| -> Result<(u32, Vec<u8>), Error> {
            let [r, w] = purgatorio::pipe()?;
            let mut child = self.sh_argv();
            child.push(String::from("-F"));
            child.extend_from_slice(&argv[1..]);
            let mut fdmap = io.clone();
            fdmap.set(READY_FD, w.as_raw_fd());
            let pid = purgatorio::spawn(SH, &child, pctl::NEWFD, &fdmap.fdmap())?;
            drop(w);
            let mut reply = Vec::new();
            File::from(r).read_to_end(&mut reply)?;
            Ok((pid, reply))
        })();
        match started {
            Ok((pid, reply)) if reply == b"R" => {
                self.set_var("apid", vec![pid.to_string()], false);
                Ok(String::new())
            }
            _ => Ok(String::from("cannot make chan")),
        }
    }

    /// The server: `sh -F file readcmd writecmd [closecmd]`. Makes the
    /// file, says so on fd 5, then runs a command for each request until
    /// the file goes away.
    pub fn serve_file2chan(&mut self, args: &[String]) -> Result<(), Error> {
        let ready = unsafe { OwnedFd::from_raw_fd(READY_FD) };
        let (Some(path), Some(rcmd), Some(wcmd)) = (args.first(), args.get(1), args.get(2)) else {
            return Err(Error::from(USAGE));
        };
        let (dir, name) = split(path);
        let fio = match file2chan::file2chan(&dir, &name) {
            Ok(f) => f,
            Err(e) => {
                let _ = file2chan::write_fd(ready.as_raw_fd(), b"E");
                return Err(Error(format!("file2chan: cannot make {path}: {e}")));
            }
        };
        file2chan::write_fd(ready.as_raw_fd(), b"R")?;
        drop(ready);
        while let Some(req) = fio.recv()? {
            if req.is_trunc() {
                let _ = req.respond_write(0);
                continue;
            }
            let cmd = if req.is_read() { rcmd } else { wcmd };
            let id = self.tags.next;
            self.tags.next += 1;
            self.tags.tags.insert(
                id,
                Tag {
                    req,
                    blocked: false,
                },
            );
            let saved = self.save_vars(&["tag"]);
            self.set_var("tag", vec![id.to_string()], true);
            let res = self.run_block(cmd);
            self.restore_vars(saved);
            if let Some(t) = self.tags.tags.remove(&id) {
                if t.blocked {
                    self.tags.tags.insert(id, t);
                } else if t.req.is_read() {
                    let _ = t.req.respond_err("invalid read");
                } else {
                    let _ = t.req.respond_write(t.req.count());
                }
            }
            if let Err(Stop::Exit(_)) = res {
                break;
            }
        }
        Ok(())
    }

    /// The tag a reply command names: its argument if `explicit`, else
    /// `$tag`.
    fn tag_id(&self, cmd: &str, arg: Option<&String>) -> Result<u64, Fail> {
        let bad = || Fail::raise("bad tag", &format!("{cmd}: cannot find tag"));
        let id = match arg {
            Some(a) => a.clone(),
            None => match self.var("tag").as_slice() {
                [t] => t.clone(),
                _ => return Err(bad()),
            },
        };
        let id: u64 = id.parse().map_err(|_| bad())?;
        if self.tags.tags.contains_key(&id) {
            Ok(id)
        } else {
            Err(bad())
        }
    }

    /// Takes the request `id` off the outstanding list to answer it,
    /// checking that it is a read (`read`) or a write.
    fn take_tag(&mut self, cmd: &str, id: u64, read: bool) -> Result<Request, Fail> {
        if self
            .tags
            .tags
            .get(&id)
            .is_some_and(|t| t.req.is_read() == read)
            && let Some(t) = self.tags.tags.remove(&id)
        {
            return Ok(t.req);
        }
        Err(Fail::raise(
            "bad tag",
            &format!("{cmd}: inappropriate tag id"),
        ))
    }

    fn peek_tag(&self, cmd: &str, id: u64, read: bool) -> Result<&Request, Fail> {
        match self.tags.tags.get(&id) {
            Some(t) if t.req.is_read() == read => Ok(&t.req),
            _ => Err(Fail::raise(
                "bad tag",
                &format!("{cmd}: inappropriate tag id"),
            )),
        }
    }

    /// The reply commands: `rblock`, `rread`, `rreadone`, `rwrite`,
    /// `rerror`, `fetchwdata` and `putrdata`.
    pub(crate) fn reply_cmd(&mut self, argv: &[String], io: &Io) -> Result<String, Fail> {
        let cmd = argv[0].as_str();
        let usage = |form: &str| Fail::raise("usage", &format!("usage: {form}"));
        match cmd {
            "rblock" | "fetchwdata" | "putrdata" => {
                if argv.len() > 2 {
                    return Err(usage(&format!("{cmd} [tag]")));
                }
                let id = self.tag_id(cmd, argv.get(1))?;
                match cmd {
                    "rblock" => {
                        if let Some(t) = self.tags.tags.get_mut(&id) {
                            t.blocked = true;
                        }
                    }
                    "fetchwdata" => {
                        let data = self.peek_tag(cmd, id, false)?.data().to_vec();
                        let _ = io.try_write(1, &data);
                    }
                    _ => {
                        let n = self.peek_tag(cmd, id, true)?.count() as usize;
                        let mut buf = Vec::with_capacity(n);
                        if let Some(fd) = io.get(0) {
                            let mut inp = crate::input::Input::fd(fd);
                            while buf.len() < n {
                                let Some(b) = inp.get() else { break };
                                buf.push(b);
                            }
                        }
                        let req = self.take_tag(cmd, id, true)?;
                        let _ = req.respond_read(&buf);
                    }
                }
            }
            "rread" | "rreadone" => {
                if !(2..=3).contains(&argv.len()) {
                    return Err(usage(&format!("{cmd} [tag] data")));
                }
                let explicit = argv.len() == 3;
                let id = self.tag_id(cmd, explicit.then(|| &argv[1]))?;
                let req = self.take_tag(cmd, id, true)?;
                let mut d = argv[argv.len() - 1].as_bytes();
                if cmd == "rreadone" {
                    d = d.get(req.offset as usize..).unwrap_or(&[]);
                }
                let _ = req.respond_read(&d[..d.len().min(req.count() as usize)]);
            }
            "rwrite" => {
                if argv.len() > 3 {
                    return Err(usage("rwrite [tag [count]]"));
                }
                let id = self.tag_id(cmd, argv.get(1))?;
                let count = match argv.get(2) {
                    Some(n) if n.bytes().all(|b| b.is_ascii_digit()) => n.parse().unwrap_or(0),
                    Some(_) => return Err(usage("rwrite [tag [count]]")),
                    None => self.peek_tag(cmd, id, false)?.count(),
                };
                let req = self.take_tag(cmd, id, false)?;
                let _ = req.respond_write(count);
            }
            _ => {
                // rerror [tag] error
                if !(2..=3).contains(&argv.len()) {
                    return Err(usage("rerror [tag] error"));
                }
                let explicit = argv.len() == 3;
                let id = self.tag_id(cmd, explicit.then(|| &argv[1]))?;
                let read = self.tags.tags.get(&id).is_some_and(|t| t.req.is_read());
                let req = self.take_tag(cmd, id, read)?;
                let _ = req.respond_err(&argv[argv.len() - 1]);
            }
        }
        Ok(String::new())
    }

    /// `${rget (data|count|offset|fid) [tag]}`.
    pub(crate) fn rget(&mut self, argv: &[String]) -> Result<Vec<String>, Fail> {
        const FORM: &str = "usage: rget (data|count|offset|fid) [tag]";
        if !(2..=3).contains(&argv.len()) {
            return Err(Fail::raise("usage", FORM));
        }
        let id = self.tag_id("rget", argv.get(2))?;
        let s = match argv[1].as_str() {
            "data" => {
                String::from_utf8_lossy(self.peek_tag("rget", id, false)?.data()).into_owned()
            }
            "count" => self.peek_tag("rget", id, true)?.count().to_string(),
            "offset" | "fid" => {
                let req = &self.tags.tags[&id].req;
                if argv[1] == "offset" {
                    req.offset.to_string()
                } else {
                    req.fid.to_string()
                }
            }
            _ => return Err(Fail::raise("usage", FORM)),
        };
        Ok(vec![s])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(split("/tmp/x"), ("/tmp".into(), "x".into()));
        assert_eq!(split("/x/"), ("/".into(), "x".into()));
    }
}
