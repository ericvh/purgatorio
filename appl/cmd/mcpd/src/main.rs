//! mcpd [-l logfile] [-n nsfile]: a Model Context Protocol server on
//! standard input and output, for driving a sandboxed Purgatorio session
//! from an LLM client.
//!
//! The transport is MCP's stdio transport: JSON-RPC 2.0, one message per
//! line. Only protocol messages are written to standard output. On emu,
//! fd 2 is the console too, which is that same output, so diagnostics go
//! to `logfile` (appended) or nowhere. See `rpc.rs` for the protocol
//! revision and `session.rs` for the sandbox. `docs/mcp.md` is the manual.
//!
//! Two progs make a server. This one, the front end, speaks the protocol
//! and never touches a file a tool names. It runs a session prog (`mcpd
//! --session`) that builds the sandbox namespace, confines itself to it and
//! runs the tools. `reset`, and a session that dies, are handled by killing
//! the session's progs and starting another.
//!
//! `-n nsfile` names a namespace file whose lines are run, as sh commands,
//! inside the new namespace before devices are locked out; it is how a
//! policy grants a workspace or the network.

mod json;
mod primer;
mod rpc;
mod session;
mod tooldefs;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::pctl;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

use json::Value;

const SELF: &str = "/bin/mcpd.wasm";
/// A longer request line is refused; it bounds the memory a client can make
/// the server hold.
const MAX_LINE: usize = 16 << 20;
/// How long a file tool may take before the session is given up on.
const FILE_TOOL_MS: i64 = 60_000;
/// What `run` may take beyond its own timeout: killing the command and
/// collecting its output.
const RUN_SLACK_MS: i64 = 15_000;
/// How long the front end waits for a killed session prog to exit.
const REAP_MS: i32 = 3000;

/// A running session prog and the pipes to it.
struct Worker {
    pid: u32,
    to: File,
    from: BufReader<File>,
}

struct Front {
    nsfile: Option<String>,
    /// The session, or why it could not be built. A failure is reported by
    /// every tool call rather than by exiting, so the client is told.
    worker: Result<Worker, String>,
    /// Process groups of the commands the session has run.
    groups: BTreeSet<u32>,
    /// Open on this prog's `/prog/n/wait`.
    wait: Option<File>,
}

impl Front {
    fn start(nsfile: Option<&str>) -> Result<Worker, String> {
        let pipe = || purgatorio::pipe().map_err(|e| format!("pipe: {e}"));
        let [req_r, req_w] = pipe()?;
        let [resp_r, resp_w] = pipe()?;
        let mut argv = vec![String::from("mcpd"), String::from("--session")];
        argv.extend(nsfile.map(String::from));
        // The session gets copies of the namespace and the environment and
        // a process group of its own. Its fd 2 is this prog's: the log.
        let pid = purgatorio::spawn(
            SELF,
            &argv,
            pctl::NEWFD | pctl::FORKNS | pctl::FORKENV | pctl::NEWPGRP,
            &[(req_r.as_raw_fd(), 0), (resp_w.as_raw_fd(), 1), (2, 2)],
        )
        .map_err(|e| format!("cannot start the session: {e}"))?;
        drop((req_r, resp_w));
        let mut w = Worker {
            pid,
            to: File::from(req_w),
            from: BufReader::new(File::from(resp_r)),
        };
        let mut line = String::new();
        let ready = w.from.read_line(&mut line).ok().filter(|&n| n > 0);
        let reply = ready.and_then(|_| json::parse(&line).ok());
        match reply {
            Some(v) if v.get("ready").is_some() => Ok(w),
            Some(v) => Err(format!(
                "cannot build the session namespace: {}",
                v.get("fatal")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )),
            None => Err(String::from("the session prog exited while starting")),
        }
    }

    /// Ends the session: kills its prog, the commands it ran and everything
    /// they started, and collects the session prog's exit.
    fn end(&mut self) {
        let Ok(w) = std::mem::replace(&mut self.worker, Err(String::from("no session"))) else {
            self.groups.clear();
            return;
        };
        let pid = w.pid;
        // Closing the pipes lets a healthy session prog exit by itself.
        drop(w);
        // Its group holds its file servers; `groups` are its commands. A
        // prog that left its group is still found through its parent.
        let mut doomed = BTreeSet::from([pid]);
        loop {
            let before = doomed.len();
            for p in session::progs("/prog") {
                if p.group == pid || self.groups.contains(&p.group) || doomed.contains(&p.ppid) {
                    doomed.insert(p.pid);
                }
            }
            if doomed.len() == before {
                break;
            }
        }
        for p in &doomed {
            let _ = std::fs::write(format!("/prog/{p}/ctl"), "kill");
        }
        self.groups.clear();
        let Some(wait) = &self.wait else { return };
        let ops = [0, wait.as_raw_fd()];
        let mut buf = [0u8; 4096];
        // SAFETY: `ops` is one (kind, fd) pair, as the call expects.
        while unsafe { purgatorio::sys::wait(ops.as_ptr(), 1, REAP_MS) } == 0 {
            let mut f: &File = wait;
            let Ok(n) = f.read(&mut buf) else { return };
            let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
            let who = msg.split_whitespace().next().and_then(|p| p.parse().ok());
            if who == Some(pid) {
                return;
            }
        }
    }

    fn restart(&mut self) {
        self.end();
        self.worker = Front::start(self.nsfile.as_deref());
        if let Err(e) = &self.worker {
            eprintln!("mcpd: {e}");
        }
    }

    /// Sends one tool call to the session prog and reads its answer.
    /// `Err(false)` means the session prog is gone, `Err(true)` that it did
    /// not answer within `limit_ms`.
    fn exchange(w: &mut Worker, request: &str, limit_ms: i64) -> Result<Value, bool> {
        w.to.write_all(request.as_bytes()).map_err(|_| false)?;
        let fd = w.from.get_ref().as_raw_fd();
        let limit = i32::try_from(limit_ms).unwrap_or(i32::MAX);
        if let Ok(None) = session::wait_readable(&[fd], limit) {
            return Err(true);
        }
        let mut line = String::new();
        match w.from.read_line(&mut line) {
            Ok(n) if n > 0 => json::parse(&line).map_err(|_| false),
            _ => Err(false),
        }
    }
}

impl rpc::Host for Front {
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        eprintln!("mcpd: call {name} {}", summary(args));
        if name == "reset" {
            self.restart();
            return match &self.worker {
                Ok(_) => Ok(json::obj([("reset", true.into())])),
                Err(e) => Err(e.clone()),
            };
        }
        let w = self.worker.as_mut().map_err(|e| e.clone())?;
        let mut request = json::obj([("name", name.into()), ("arguments", args.clone())]).encode();
        request.push('\n');
        // `run` enforces its own timeout; this one is the backstop for a
        // session prog that is stuck, such as a file tool reading a file
        // that never answers.
        let limit = match name {
            "run" => {
                let t = args.get("timeout_ms").and_then(Value::as_i64);
                t.unwrap_or(tooldefs::DEFAULT_TIMEOUT_MS) + RUN_SLACK_MS
            }
            _ => FILE_TOOL_MS,
        };
        let reply = match Front::exchange(w, &request, limit) {
            Ok(reply) => reply,
            Err(stuck) => {
                self.restart();
                let why = if stuck {
                    "the tool did not finish in time (a file that blocks when read or written?)"
                } else {
                    "the session ended while the tool was running (its server prog was killed)"
                };
                return Err(format!(
                    "{why}; a new session has been started, with an empty /tmp and a fresh namespace"
                ));
            }
        };
        if let Some(g) = reply.get("group").and_then(Value::as_i64) {
            self.groups.insert(g as u32);
        }
        match (reply.get("result"), reply.get("error")) {
            (Some(v), _) => Ok(v.clone()),
            (None, Some(e)) => Err(String::from(e.as_str().unwrap_or("tool failed"))),
            (None, None) => Err(String::from("the session sent a bad reply")),
        }
    }
}

/// The arguments of a call, cut short, for the log.
fn summary(args: &Value) -> String {
    let text = args.encode();
    match text.char_indices().nth(200) {
        Some((i, _)) => format!("{}...", &text[..i]),
        None => text,
    }
}

/// Points fd 2 at the log file, or at `/dev/null` without one.
fn redirect_stderr(log: Option<&str>) -> Result<(), Error> {
    let fd = match log {
        Some(path) => {
            let fd = match purgatorio::open(path, OWRITE) {
                Ok(fd) => fd,
                Err(_) => purgatorio::create(path, OWRITE, 0o666)
                    .map_err(|e| Error(format!("{path}: {e}")))?,
            };
            let mut f = File::from(fd);
            f.seek(SeekFrom::End(0))?;
            OwnedFd::from(f)
        }
        None => purgatorio::open("/dev/null", OWRITE)?,
    };
    purgatorio::dup(fd.as_raw_fd(), 2)?;
    Ok(())
}

/// Reads one line of at most [`MAX_LINE`] bytes. `Ok(None)` is end of
/// input; `Ok(Some(false))` is a line that was too long and was skipped.
fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<Option<bool>> {
    line.clear();
    let n = input
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE && line.last() != Some(&b'\n') {
        // Drop the rest of the line without keeping it.
        loop {
            line.clear();
            let n = input.by_ref().take(1 << 16).read_until(b'\n', line)?;
            if n == 0 || line.last() == Some(&b'\n') {
                return Ok(Some(false));
            }
        }
    }
    Ok(Some(true))
}

fn serve(nsfile: Option<String>) -> Result<(), Error> {
    let wait = purgatorio::open(&format!("#p/{}/wait", purgatorio::pid()), OREAD)
        .ok()
        .map(File::from);
    let mut front = Front {
        worker: Front::start(nsfile.as_deref()),
        nsfile,
        groups: BTreeSet::new(),
        wait,
    };
    match &front.worker {
        Ok(w) => eprintln!("mcpd: session prog {} started", w.pid),
        Err(e) => eprintln!("mcpd: {e}"),
    }
    let mut server = rpc::Server::default();
    let mut input = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout().lock();
    let mut line = Vec::new();
    while let Some(whole) = read_line(&mut input, &mut line)? {
        let reply = if !whole {
            Some(rpc::too_long())
        } else {
            match std::str::from_utf8(&line) {
                Ok(text) => server.handle(text, &mut front),
                Err(_) => server.handle("\u{1}", &mut front),
            }
        };
        if let Some(mut reply) = reply {
            reply.push('\n');
            output.write_all(reply.as_bytes())?;
            output.flush()?;
        }
    }
    eprintln!("mcpd: end of input");
    front.end();
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--feed") => {
            session::feeder();
            return Ok(());
        }
        Some("--session") => {
            let policy = args.get(2).map(|path| {
                std::fs::read_to_string(path).map_err(|e| format!("{path}: {}", Error::from(e)))
            });
            let built = policy
                .transpose()
                .and_then(|p| session::Session::build(p.as_deref()));
            return match built {
                Ok(mut s) => s.serve(),
                Err(e) => {
                    let mut line = json::obj([("fatal", e.as_str().into())]).encode();
                    line.push('\n');
                    // SAFETY: fd 1 is the pipe to the front end.
                    let mut out = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(1) };
                    let _ = out.write_all(line.as_bytes());
                    purgatorio::exits(&format!("fail:{e}"))
                }
            };
        }
        _ => {}
    }
    let mut arg = Arg::new(args, "mcpd [-l logfile] [-n nsfile]");
    let (mut log, mut nsfile) = (None, None);
    while let Some(c) = arg.opt() {
        match c {
            'l' => log = Some(arg.earg()?),
            'n' => nsfile = Some(arg.earg()?),
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error::from("usage: mcpd [-l logfile] [-n nsfile]"));
    }
    redirect_stderr(log.as_deref())?;
    serve(nsfile)
}
