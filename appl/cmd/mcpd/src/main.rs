//! mcpd [-l logfile] [-n nsfile] [-t filetimeout] [-M memory] [-P progs]: a Model Context Protocol server on
//! standard input and output, for driving a sandboxed Purgatorio session
//! from an LLM client.
//!
//! The transport is MCP's stdio transport: JSON-RPC 2.0, one message per
//! line. Only protocol messages are written to standard output. On emu,
//! fd 2 is the console too, which is that same output, so the audit log
//! goes to `logfile` (appended) or nowhere. See `rpc.rs` for the protocol
//! revision and `session.rs` for the sandbox. `docs/mcp.md` is the manual.
//!
//! Three progs make a server. This one, the front end, speaks the protocol,
//! writes the log and never touches a file a tool names. A relay prog
//! copies the client's input to a pipe, which, unlike the console, can be
//! waited on together with the session. The session prog (`mcpd
//! --session`) builds the sandbox namespace, confines itself to it and
//! answers the calls, file tools through a worker prog per call (`mcpd
//! --file`, limited by `-t`). Calls finish out of order, so the front end matches
//! answers to requests by tag. `reset`, and a session that dies, are
//! handled by killing the session's progs and starting another.
//!
//! A session is a prog domain (`#pdomain`): the session prog and every prog
//! started from it, whatever their process groups. `-M` and `-P` limit the
//! wasm memory the domain's progs may hold between them and how many there
//! may be. The front end ends a session by killing every member of its
//! domain, which it sees through `#pdomain=<session pid>`.
//!
//! `-n nsfile` names a namespace(6) file that `nsbuild` applies in the new
//! namespace before devices are locked out; it is how a policy grants a
//! workspace or the network.

mod files;
mod json;
mod primer;
mod procs;
mod rpc;
mod session;
mod tooldefs;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::pctl;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

use json::Value;
use session::{ms_until, wait_readable};

const SELF: &str = "/bin/mcpd.wasm";
const USAGE: &str = "mcpd [-l logfile] [-n nsfile] [-t filetimeout] [-M memory] [-P progs]";
/// A longer request line is refused; it bounds the memory a client can make
/// the server hold.
const MAX_LINE: usize = 16 << 20;
/// How long a file tool may take by default before the session abandons
/// it (`-t`).
const FILE_TOOL_MS: i64 = 30_000;
/// What the front end allows the session beyond a call's own limit before
/// it gives up on the session.
const SESSION_SLACK_MS: i64 = 15_000;
/// What a command may take beyond its own timeout: killing it and
/// collecting its output.
const RUN_SLACK_MS: i64 = 15_000;
/// How long the front end waits for a killed session prog to exit.
const REAP_MS: i32 = 3000;
/// Bytes written to the session and not yet read by it. The session reads
/// as much at a time, and a pipe holds more, so a write never blocks.
const WINDOW: usize = 16 * 1024;
/// The wasm memory a session's progs may hold between them by default
/// (`-M`). An idle session holds about 2 MiB.
const SESSION_MEM: u64 = 256 << 20;
/// How many progs a session may have by default (`-P`). An idle session
/// has six; each command and job adds a launcher and what it runs.
const SESSION_PROGS: u64 = 128;

/// A running session prog and the pipes to it.
struct Worker {
    pid: u32,
    to: File,
    from: File,
    /// What has come from the session and is not yet a whole line.
    acc: Vec<u8>,
    /// Bytes written to `to` that the session has not acknowledged.
    inflight: usize,
}

/// When a call came, and its place among the replies.
#[derive(Clone, Copy)]
struct Ticket {
    received: Instant,
    /// `None` for a `job_read` that waits, whose reply goes out whenever
    /// it is ready.
    seq: Option<u64>,
}

/// A call the session has not answered.
struct Pending {
    call: rpc::Call,
    ticket: Ticket,
    /// The session's limit once it begins the call; `None` until then.
    deadline: Option<Instant>,
}

/// What the front end gives every session.
struct Config {
    nsfile: Option<String>,
    /// The session's limit on a file tool, in milliseconds (`-t`).
    file_ms: i64,
    limits: session::Limits,
}

struct Front {
    config: Config,
    /// The session, or why it could not be built. A failure is reported by
    /// every tool call rather than by exiting, so the client is told.
    worker: Result<Worker, String>,
    /// Open on this prog's `/prog/n/wait`.
    wait: Option<File>,
    pending: BTreeMap<u64, Pending>,
    next_tag: u64,
    /// Requests for the session, written as the window allows.
    outq: VecDeque<u8>,
    /// Calls held back behind a `reset`, which waits for the calls before
    /// it.
    held: VecDeque<(rpc::Call, Ticket)>,
    /// Replies in the order of the requests, each sent once it and all
    /// before it are ready, so that a client that sends many requests at
    /// once reads the answers in order. A `job_read` that waits has no
    /// place here.
    order: VecDeque<(u64, Option<String>)>,
    next_seq: u64,
    stdout: std::io::Stdout,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A string argument, cut short for the log.
fn shorten(v: &Value) -> Value {
    match v {
        Value::Str(s) if s.chars().count() > 64 => {
            let cut: String = s.chars().take(48).collect();
            Value::Str(format!("{cut}...({} bytes)", s.len()))
        }
        Value::Array(a) => Value::Array(a.iter().map(shorten).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), shorten(v))).collect()),
        v => v.clone(),
    }
}

/// The arguments of a call as JSON, each long string cut and the whole cut
/// at 400 characters.
fn summary(args: &Value) -> String {
    let text = shorten(args).encode();
    match text.char_indices().nth(400) {
        Some((i, _)) => format!("{}...", &text[..i]),
        None => text,
    }
}

/// Appends a line to the log: seconds since 1970, what happened, details.
fn log(what: &str) {
    eprintln!("{} {what}", now_secs());
}

fn log_call(call: &rpc::Call, status: &str, received: Instant) {
    log(&format!(
        "{} {} {status} {}ms {}",
        call.kind(),
        call.name,
        received.elapsed().as_millis(),
        summary(&call.args)
    ));
}

/// How long the session may take over a call once it has begun it.
/// `file_ms` is the session's own limit on a file tool.
fn limit(call: &rpc::Call, file_ms: i64) -> i64 {
    let arg = |k: &str| call.args.get(k).and_then(Value::as_i64);
    match call.name.as_str() {
        "run" => arg("timeout_ms").unwrap_or(tooldefs::DEFAULT_TIMEOUT_MS) + RUN_SLACK_MS,
        "mount" => arg("timeout_ms").unwrap_or(tooldefs::MOUNT_TIMEOUT_MS) + RUN_SLACK_MS,
        "job_read" => arg("wait_ms").unwrap_or(0) + SESSION_SLACK_MS,
        _ => file_ms + SESSION_SLACK_MS,
    }
}

/// A `job_read` that waits: it does not hold up a `reset`.
fn waits(call: &rpc::Call) -> bool {
    call.name == "job_read" && call.args.get("wait_ms").and_then(Value::as_i64) > Some(0)
}

impl Front {
    fn start(config: &Config) -> Result<Worker, String> {
        let pipe = || purgatorio::pipe().map_err(|e| format!("pipe: {e}"));
        let [req_r, req_w] = pipe()?;
        let [resp_r, resp_w] = pipe()?;
        let null = purgatorio::open("/dev/null", OWRITE).map_err(|e| format!("/dev/null: {e}"))?;
        let mut argv = vec![
            String::from("mcpd"),
            String::from("--session"),
            config.file_ms.to_string(),
            config.limits.mem.to_string(),
            config.limits.progs.to_string(),
        ];
        argv.extend(config.nsfile.clone());
        // The session gets copies of the namespace and the environment and
        // a process group of its own. Its fd 2 is not the log, which it
        // could otherwise write through /fd/2.
        let pid = purgatorio::spawn(
            SELF,
            &argv,
            pctl::NEWFD | pctl::FORKNS | pctl::FORKENV | pctl::NEWPGRP,
            &[
                (req_r.as_raw_fd(), 0),
                (resp_w.as_raw_fd(), 1),
                (null.as_raw_fd(), 2),
            ],
        )
        .map_err(|e| format!("cannot start the session: {e}"))?;
        drop((req_r, resp_w, null));
        let mut w = Worker {
            pid,
            to: File::from(req_w),
            from: File::from(resp_r),
            acc: Vec::new(),
            inflight: 0,
        };
        if let Err(e) = Front::ready(&mut w) {
            // The file servers a half-built session started would
            // otherwise outlive it, and keep the emu running.
            drop(w);
            kill_domain(pid);
            return Err(e);
        }
        Ok(w)
    }

    /// Reads the session prog's first line: `ready`, or why it failed.
    fn ready(w: &mut Worker) -> Result<(), String> {
        let mut buf = [0u8; 4096];
        while !w.acc.contains(&b'\n') {
            match w.from.read(&mut buf) {
                Ok(n) if n > 0 => w.acc.extend_from_slice(&buf[..n]),
                _ => return Err(String::from("the session prog exited while starting")),
            }
        }
        let i = w.acc.iter().position(|&b| b == b'\n').unwrap_or(0);
        let line: Vec<u8> = w.acc.drain(..=i).collect();
        match json::parse(&String::from_utf8_lossy(&line)) {
            Ok(v) if v.get("ready").is_some() => Ok(()),
            Ok(v) => Err(format!(
                "cannot build the session namespace: {}",
                v.get("fatal")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )),
            Err(_) => Err(String::from("the session sent a bad reply")),
        }
    }

    /// Ends the session: kills every prog in its domain (the session prog,
    /// its file servers, the commands it ran and everything they started,
    /// whatever their groups and whether or not their parents live), and
    /// collects the session prog's exit.
    fn end(&mut self) {
        self.outq.clear();
        let Ok(w) = std::mem::replace(&mut self.worker, Err(String::from("no session"))) else {
            return;
        };
        let pid = w.pid;
        // Closing the pipes lets a healthy session prog exit by itself.
        drop(w);
        kill_domain(pid);
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

    /// Replaces the session. Calls in progress fail with `why`.
    fn restart(&mut self, why: &str) {
        self.end();
        self.fail_pending(why);
        self.worker = Front::start(&self.config);
        match &self.worker {
            Ok(w) => log(&format!("session start {}", w.pid)),
            Err(e) => log(&format!("session failed {e}")),
        }
    }

    fn fail_pending(&mut self, why: &str) {
        for (_, p) in std::mem::take(&mut self.pending) {
            self.answer(&p.call, Err(String::from(why)), p.ticket);
        }
    }

    fn write_out(&mut self, line: &str) {
        let _ = self.stdout.write_all(line.as_bytes());
        let _ = self.stdout.write_all(b"\n");
        let _ = self.stdout.flush();
    }

    /// A place among the replies for a request that has come.
    fn ticket(&mut self, call: Option<&rpc::Call>) -> Ticket {
        let seq = if call.is_some_and(waits) {
            None
        } else {
            let seq = self.next_seq;
            self.next_seq += 1;
            self.order.push_back((seq, None));
            Some(seq)
        };
        Ticket {
            received: Instant::now(),
            seq,
        }
    }

    /// Sends a reply when the replies before it have gone.
    fn deliver(&mut self, seq: Option<u64>, reply: String) {
        let Some(seq) = seq else {
            self.write_out(&reply);
            return;
        };
        if let Some(slot) = self.order.iter_mut().find(|s| s.0 == seq) {
            slot.1 = Some(reply);
        }
        while let Some((_, Some(_))) = self.order.front() {
            if let Some((_, Some(r))) = self.order.pop_front() {
                self.write_out(&r);
            }
        }
    }

    /// Replies to a request that needs no call.
    fn reply_now(&mut self, reply: String) {
        let t = self.ticket(None);
        self.deliver(t.seq, reply);
    }

    fn answer(&mut self, call: &rpc::Call, outcome: Result<Value, String>, t: Ticket) {
        log_call(
            call,
            if outcome.is_ok() { "ok" } else { "error" },
            t.received,
        );
        let reply = call.finish(outcome);
        self.deliver(t.seq, reply);
    }

    /// Takes a call from the client: answers it, or sends it to the
    /// session. `held` says the call was held behind a `reset` and is now
    /// free to go.
    fn submit(&mut self, call: rpc::Call, t: Ticket, held: bool) {
        if let Some(why) = &call.rejected {
            log_call(&call, "invalid", t.received);
            let reply = call.finish(Err(why.clone()));
            self.deliver(t.seq, reply);
            return;
        }
        let reset = call.what == rpc::What::Tool && call.name == "reset";
        if (!held && !self.held.is_empty())
            || (reset && self.pending.values().any(|p| !waits(&p.call)))
        {
            self.held.push_back((call, t));
            return;
        }
        if reset {
            log("session reset");
            self.restart("the session was reset before the call finished");
            let outcome = match &self.worker {
                Ok(_) => Ok(json::obj([("reset", true.into())])),
                Err(e) => Err(e.clone()),
            };
            self.answer(&call, outcome, t);
            return;
        }
        if let Err(e) = &self.worker {
            let e = e.clone();
            self.answer(&call, Err(e), t);
            return;
        }
        let tag = self.next_tag;
        self.next_tag += 1;
        let mut line = json::obj([
            ("tag", tag.into()),
            ("name", call.name.as_str().into()),
            ("arguments", call.args.clone()),
        ])
        .encode();
        line.push('\n');
        self.outq.extend(line.as_bytes());
        self.pending.insert(
            tag,
            Pending {
                call,
                ticket: t,
                deadline: None,
            },
        );
    }

    /// Handles one line from the client.
    fn take(&mut self, server: &mut rpc::Server, line: &[u8]) {
        // A line that is not UTF-8 gets the parse error.
        let step = server.begin(std::str::from_utf8(line).unwrap_or("\u{1}"));
        match step {
            rpc::Step::Quiet => {}
            rpc::Step::Reply(r) => self.reply_now(r),
            rpc::Step::Call(c) => {
                let t = self.ticket(Some(&c));
                self.submit(c, t, false);
            }
        }
    }

    /// Submits the calls held behind a `reset` once it can go.
    fn release(&mut self) {
        while let Some((call, _)) = self.held.front() {
            let reset = call.name == "reset";
            if reset && self.pending.values().any(|p| !waits(&p.call)) {
                return;
            }
            if let Some((call, t)) = self.held.pop_front() {
                self.submit(call, t, true);
            }
        }
    }

    /// Writes what the window allows.
    fn flush(&mut self) {
        let Ok(w) = &mut self.worker else { return };
        while w.inflight < WINDOW && !self.outq.is_empty() {
            let n = (WINDOW - w.inflight).min(self.outq.len());
            let chunk: Vec<u8> = self.outq.drain(..n).collect();
            if w.to.write_all(&chunk).is_err() {
                self.restart(
                    "the session ended while the tool was running (its server prog was killed); \
                     a new session has been started, with an empty /tmp and a fresh namespace",
                );
                return;
            }
            w.inflight += n;
        }
    }

    /// Reads what the session has sent.
    fn read_session(&mut self) {
        let Ok(w) = &mut self.worker else { return };
        let mut buf = vec![0u8; 65536];
        let n = w.from.read(&mut buf).unwrap_or(0);
        if n == 0 {
            self.restart(
                "the session ended while the tool was running (its server prog was killed); \
                 a new session has been started, with an empty /tmp and a fresh namespace",
            );
            return;
        }
        w.acc.extend_from_slice(&buf[..n]);
        let mut lines = Vec::new();
        while let Some(i) = w.acc.iter().position(|&b| b == b'\n') {
            lines.push(w.acc.drain(..=i).collect::<Vec<u8>>());
        }
        for line in lines {
            let Ok(v) = json::parse(&String::from_utf8_lossy(&line)) else {
                continue;
            };
            if let Some(n) = v.get("ack").and_then(Value::as_i64) {
                if let Ok(w) = &mut self.worker {
                    w.inflight = w.inflight.saturating_sub(n.max(0) as usize);
                }
                continue;
            }
            if let Some(tag) = v.get("start").and_then(Value::as_i64) {
                if let Some(p) = self.pending.get_mut(&(tag as u64)) {
                    let ms = limit(&p.call, self.config.file_ms).max(0) as u64;
                    p.deadline = Some(Instant::now() + std::time::Duration::from_millis(ms));
                }
                continue;
            }
            let Some(tag) = v.get("tag").and_then(Value::as_i64) else {
                continue;
            };
            let Some(p) = self.pending.remove(&(tag as u64)) else {
                continue;
            };
            let outcome = match (v.get("result"), v.get("error")) {
                (Some(r), _) => Ok(r.clone()),
                (None, Some(e)) => Err(String::from(e.as_str().unwrap_or("tool failed"))),
                (None, None) => Err(String::from("the session sent a bad reply")),
            };
            self.answer(&p.call, outcome, p.ticket);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().filter_map(|p| p.deadline).min()
    }

    /// Gives up on a session that has not answered a call in time.
    fn check_deadlines(&mut self) {
        let now = Instant::now();
        if self.next_deadline().is_some_and(|d| now >= d) {
            self.restart(
                "the session did not answer in time; \
                 a new session has been started, with an empty /tmp and a fresh namespace",
            );
        }
    }
}

/// Kills every prog in the domain founded by the session prog `pid`, until
/// none is left or [`REAP_MS`] has passed: a prog may start another while
/// it is being killed. The domain is named after the process group that
/// founded it, the session prog's own.
fn kill_domain(pid: u32) {
    let view = format!("#pdomain={pid}");
    if purgatorio::stat(&view).is_err() {
        // No view of the domain (this prog has given up devices): kill the
        // session's group and what descends from it.
        kill_descendants(pid);
        return;
    }
    let start = Instant::now();
    loop {
        let members = procs::progs(&view);
        if members.is_empty() || start.elapsed().as_millis() > REAP_MS as u128 {
            return;
        }
        for p in members {
            let _ = std::fs::write(format!("{view}/{}/ctl", p.pid), "kill");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Kills the progs of group `pid` and their descendants, as far as
/// `/prog` shows them.
fn kill_descendants(pid: u32) {
    let mut doomed = BTreeSet::from([pid]);
    loop {
        let before = doomed.len();
        for p in procs::progs("/prog") {
            if p.group == pid || doomed.contains(&p.ppid) {
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

/// Starts the relay and returns the pipe it writes the client's input to.
fn relay() -> Result<File, Error> {
    let [r, w] = purgatorio::pipe()?;
    purgatorio::spawn(
        SELF,
        &[String::from("mcpd"), String::from("--relay")],
        pctl::NEWFD,
        &[(0, 0), (w.as_raw_fd(), 1)],
    )?;
    Ok(File::from(r))
}

fn serve(config: Config) -> Result<(), Error> {
    let wait = purgatorio::open(&format!("#p/{}/wait", purgatorio::pid()), OREAD)
        .ok()
        .map(File::from);
    let mut input = relay()?;
    let mut front = Front {
        worker: Front::start(&config),
        config,
        wait,
        pending: BTreeMap::new(),
        next_tag: 1,
        outq: VecDeque::new(),
        held: VecDeque::new(),
        order: VecDeque::new(),
        next_seq: 0,
        stdout: std::io::stdout(),
    };
    match &front.worker {
        Ok(w) => log(&format!("session start {}", w.pid)),
        Err(e) => log(&format!("session failed {e}")),
    }
    let mut server = rpc::Server::default();
    let mut acc: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut eof = false;
    let mut buf = vec![0u8; 65536];
    loop {
        if eof && front.pending.is_empty() && front.held.is_empty() {
            break;
        }
        let mut fds = Vec::new();
        if !eof {
            fds.push(input.as_raw_fd());
        }
        if let Ok(w) = &front.worker {
            fds.push(w.from.as_raw_fd());
        }
        if fds.is_empty() {
            break;
        }
        match wait_readable(&fds, ms_until(front.next_deadline())) {
            Ok(Some(0)) if !eof => {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    eof = true;
                }
                acc.extend_from_slice(&buf[..n]);
                loop {
                    let Some(i) = acc.iter().position(|&b| b == b'\n') else {
                        if acc.len() > MAX_LINE {
                            // Drop the rest of the line without keeping it.
                            acc.clear();
                            if !skipping {
                                skipping = true;
                                front.reply_now(rpc::too_long());
                            }
                        }
                        break;
                    };
                    let line: Vec<u8> = acc.drain(..=i).collect();
                    if std::mem::take(&mut skipping) {
                        continue;
                    }
                    if line.len() > MAX_LINE + 1 {
                        front.reply_now(rpc::too_long());
                        continue;
                    }
                    front.take(&mut server, &line);
                }
                // A last line without a newline.
                if eof && !acc.is_empty() && !skipping {
                    let line = std::mem::take(&mut acc);
                    front.take(&mut server, &line);
                }
            }
            Ok(Some(_)) => front.read_session(),
            Ok(None) => {}
            Err(_) => break,
        }
        front.check_deadlines();
        front.release();
        front.flush();
    }
    log("session end");
    front.end();
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--feed") => {
            procs::feeder();
            return Ok(());
        }
        Some("--relay") => {
            procs::relay();
            return Ok(());
        }
        Some("--file") => {
            session::file_worker();
            return Ok(());
        }
        Some("--exec") => procs::exec(&args[2..]),
        Some("--session") => {
            let num = |i: usize, default: u64| {
                args.get(i).and_then(|t| t.parse().ok()).unwrap_or(default)
            };
            let file_ms = num(2, FILE_TOOL_MS as u64);
            let limits = session::Limits {
                mem: num(3, SESSION_MEM),
                progs: num(4, SESSION_PROGS),
            };
            let policy = args.get(5).map(|path| {
                std::fs::read_to_string(path).map_err(|e| format!("{path}: {}", Error::from(e)))
            });
            let built = policy
                .transpose()
                .and_then(|p| session::Session::build(p.as_deref(), file_ms, limits));
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
    let mut arg = Arg::new(args, USAGE);
    let usage = || Error(format!("usage: {USAGE}"));
    let mut log = None;
    let mut config = Config {
        nsfile: None,
        file_ms: FILE_TOOL_MS,
        limits: session::Limits {
            mem: SESSION_MEM,
            progs: SESSION_PROGS,
        },
    };
    while let Some(c) = arg.opt() {
        match c {
            'l' => log = Some(arg.earg()?),
            'n' => config.nsfile = Some(arg.earg()?),
            't' => {
                config.file_ms = arg
                    .earg()?
                    .parse()
                    .ok()
                    .filter(|&t| t > 0)
                    .ok_or_else(usage)?;
            }
            'M' => {
                config.limits.mem = parse_size(&arg.earg()?)
                    .filter(|&n| n > 0)
                    .ok_or_else(usage)?;
            }
            'P' => {
                config.limits.progs = arg
                    .earg()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or_else(usage)?;
            }
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(usage());
    }
    redirect_stderr(log.as_deref())?;
    serve(config)
}

/// A byte count: digits, then optionally `k`, `m` or `g` for 2^10, 2^20 or
/// 2^30 times as many, as `/prog/n/ctl` takes them.
fn parse_size(s: &str) -> Option<u64> {
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

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("256m"), Some(256 << 20));
        assert_eq!(parse_size("4K"), Some(4096));
        assert_eq!(parse_size("1g"), Some(1 << 30));
        assert_eq!(parse_size("100"), Some(100));
        for bad in ["", "m", "1x", "-1", "1.5m"] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }
}
