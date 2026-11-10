//! The session prog: `mcpd --session [nsfile]`.
//!
//! The front end starts one of these per session with a forked namespace
//! and environment and a new process group. It builds the sandbox, gives up
//! the right to attach devices, and then serves requests. Everything a tool
//! touches is named in the sandbox, because this prog has no other
//! namespace.
//!
//! The sandbox, in order:
//!
//! 1. A fresh `memfs` becomes the root (`pctl(NEWNS)` with it as the
//!    current directory), holding only empty mount points. The host tree
//!    (`#U` on emu) is not in it.
//! 2. `/bin` and `/lib` are mounted from read-only exports (`export -r`) of
//!    the directories the server itself sees. The export refuses every
//!    request that changes a file, so nothing the session does can modify
//!    them, and the only way to reach those trees is through that mount.
//! 3. `/dev` holds `null`, `random`, `time`, `user`, `sysname`, `pid`,
//!    `memory` and `drivers` from `#c`, and a `cons` that is `#c/null`: the
//!    real console is the MCP connection. `/env` is `#e`, `/prog` is `#p`
//!    and `/fd` is `#d`.
//! 4. `/tmp` is a second `memfs`, private to the session.
//! 5. The namespace file, if one was given, is applied by `nsbuild`. This
//!    is where a policy grants more: `bind -c '#U/work' /n/work`,
//!    `bind '#I' /net`. Missing mount points are made first.
//! 6. `pctl(NODEVS)`: from here on neither this prog nor anything it starts
//!    can name a device with `#`, except the ones Inferno always allows
//!    (`#e`, `#s`, `#|`, `#M`, `#¤`).
//!
//! The protocol with the front end, on fds 0 and 1, is one JSON object per
//! line. A request is `{"tag", "name", "arguments"}`; the session answers
//! `{"tag", "result"}` or `{"tag", "error"}`, with `"group"` when it
//! started a process group the front end must kill at the end. It sends
//! `{"start": tag}` when it begins a request, and `{"ack": n}` for every
//! `n` bytes it reads, which is how the front end knows the request pipe
//! has room and never blocks writing to it.
//!
//! Requests are begun in order, one at a time: a `run` or a `mount` holds
//! up the ones behind it until it ends. A `job_read` that waits does not;
//! it is answered when output comes, the job exits or the wait is over.
//! Meanwhile the session keeps reading every job's output.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::mount::{MAFTER, MBEFORE, MCREATE, MNOAUTH, MREPL};
use purgatorio::abi::pctl;
use purgatorio::{DMDIR, Error};

use crate::files;
use crate::json::{self, Value, base64_decode, obj};
use crate::procs::{Job, Kind, Proc, feed, start_feeder};
use crate::tooldefs::{DEFAULT_OUTPUT, DEFAULT_TIMEOUT_MS, JOB_BUFFER, MAX_JOBS, MOUNT_TIMEOUT_MS};

const SH: &str = "/bin/sh.wasm";
/// Where relative names start and where commands start by default.
pub const HOME: &str = "/tmp";
/// Mount points in the new root. `net`, `chan`, `n` and `mnt` are empty
/// unless the namespace file fills them.
const DIRS: [&str; 11] = [
    "bin", "lib", "tmp", "dev", "env", "prog", "fd", "net", "chan", "n", "mnt",
];
/// `#c` files a session may use. The console, its control file, `kprint`
/// and `sysctl` are left out.
const DEVS: [&str; 8] = [
    "null", "random", "time", "user", "sysname", "pid", "memory", "drivers",
];
/// The most a request line from the front end is read at a time; the
/// front end's window is no larger.
const READ_CHUNK: usize = 16 * 1024;

pub fn err(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

fn null(m: u8) -> Result<OwnedFd, String> {
    purgatorio::open("/dev/null", m).map_err(|e| err("/dev/null", e))
}

/// `purgatorio.wait` on fds that would read without blocking. `Ok(None)`
/// is a timeout.
pub fn wait_readable(fds: &[i32], timeout_ms: i32) -> Result<Option<usize>, Error> {
    let ops: Vec<i32> = fds.iter().flat_map(|&fd| [0, fd]).collect();
    let before = Instant::now();
    // SAFETY: `ops` holds `fds.len()` pairs of i32, as the call expects.
    let r = unsafe { purgatorio::sys::wait(ops.as_ptr(), fds.len(), timeout_ms) };
    if r >= 0 {
        return Ok(Some(r as usize));
    }
    // A timeout and an error both return -1; only an error comes back early.
    if timeout_ms >= 0 && before.elapsed() >= Duration::from_millis(timeout_ms as u64) {
        return Ok(None);
    }
    let e = Error::last();
    if timeout_ms == 0 || e.0.is_empty() {
        Ok(None)
    } else {
        Err(e)
    }
}

/// Milliseconds from now until `at`, for a wait; -1 is forever.
pub fn ms_until(at: Option<Instant>) -> i32 {
    match at {
        None => -1,
        Some(at) => {
            let d = at.saturating_duration_since(Instant::now());
            // Round up, so that the wait does not end just short of `at`.
            i32::try_from(d.as_millis() + 1).unwrap_or(i32::MAX)
        }
    }
}

struct Request {
    tag: Value,
    name: String,
    args: Value,
}

/// A `job_read` waiting for output.
struct Parked {
    tag: Value,
    job: u32,
    from: [Option<u64>; 2],
    max: usize,
    deadline: Instant,
}

pub struct Session {
    /// Open on this prog's `/prog/n/wait`.
    wait: File,
    out: Option<File>,
    queue: VecDeque<Request>,
    /// A `run` or `mount` is in progress; the queue waits for it.
    busy: bool,
    procs: Vec<Proc>,
    next_job: u32,
    /// Feeders writing to jobs' input: (feeder pid, job id).
    feeders: Vec<(u32, u32)>,
    parked: Vec<Parked>,
}

impl Session {
    /// Reads exit messages until the one for `pid`; returns its status.
    /// Only used while building, before there is anything else to wait for.
    fn reap(&mut self, pid: u32) -> Result<String, String> {
        loop {
            let mut buf = [0u8; 4096];
            let n = self
                .wait
                .read(&mut buf)
                .map_err(|e| err("wait", Error::from(e)))?;
            let fields = purgatorio::unquote_fields(&String::from_utf8_lossy(&buf[..n]));
            if let [p, _, status] = fields.as_slice()
                && p.parse() == Ok(pid)
            {
                return Ok(status.clone());
            }
        }
    }

    /// Runs a helper command to completion with `stdin` as its input. Its
    /// diagnostics are the error.
    fn helper(&mut self, cmd: &str, argv: &[&str], stdin: OwnedFd) -> Result<(), String> {
        let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
        let [err_r, err_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let pid = purgatorio::spawn(
            cmd,
            &argv,
            pctl::NEWFD,
            &[
                (stdin.as_raw_fd(), 0),
                (err_w.as_raw_fd(), 1),
                (err_w.as_raw_fd(), 2),
            ],
        )
        .map_err(|e| err(&argv.join(" "), e))?;
        drop((stdin, err_w));
        // A file server the helper starts keeps the pipe; read what is
        // there when the helper itself is gone.
        let status = self.reap(pid)?;
        let mut text = Vec::new();
        let mut f = File::from(err_r);
        while let Ok(Some(_)) = wait_readable(&[f.as_raw_fd()], 0) {
            let mut buf = [0u8; 4096];
            match f.read(&mut buf) {
                Ok(n) if n > 0 => text.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        if status.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&text);
        Err(match text.trim() {
            "" => err(&argv.join(" "), status),
            t => String::from(t),
        })
    }

    /// Starts `export -r dir` on a pipe and returns the client end. The
    /// export keeps the namespace it was started in, so it must start before
    /// the root changes.
    fn readonly_export(dir: &str) -> Result<OwnedFd, String> {
        let [a, b] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let argv = ["export", "-A", "-r", dir].map(String::from);
        purgatorio::spawn(
            "/bin/export.wasm",
            &argv,
            pctl::NEWFD | pctl::FORKNS,
            &[(a.as_raw_fd(), 0), (2, 2)],
        )
        .map_err(|e| err("export", e))?;
        Ok(b)
    }

    /// Builds the sandbox described at the top of this file. `policy` is the
    /// text of the namespace file.
    pub fn build(policy: Option<&str>) -> Result<Session, String> {
        let me = purgatorio::pid();
        let wait =
            purgatorio::open(&format!("#p/{me}/wait"), OREAD).map_err(|e| err("wait file", e))?;
        let mut s = Session {
            wait: File::from(wait),
            out: None,
            queue: VecDeque::new(),
            busy: false,
            procs: Vec::new(),
            next_job: 1,
            feeders: Vec::new(),
            parked: Vec::new(),
        };
        // Any directory will do to hold the new root while it is built; the
        // namespace it is in is about to be thrown away.
        let stage = ["/tmp", "/mnt", "/n", "/chan"]
            .into_iter()
            .find(|d| purgatorio::stat(d).is_ok_and(|d| d.mode & DMDIR != 0))
            .ok_or("no directory to build the session root on")?;
        s.helper("/bin/memfs.wasm", &["memfs", stage], null(OREAD)?)?;
        for d in DIRS {
            let path = format!("{stage}/{d}");
            purgatorio::create(&path, OREAD, DMDIR | 0o777).map_err(|e| err(&path, e))?;
        }
        for f in DEVS.iter().chain(&["cons"]) {
            let path = format!("{stage}/dev/{f}");
            purgatorio::create(&path, OWRITE, 0o666).map_err(|e| err(&path, e))?;
        }
        let bin = Session::readonly_export("/bin")?;
        let lib = Session::readonly_export("/lib")?;
        purgatorio::chdir(stage).map_err(|e| err(stage, e))?;
        purgatorio::pctl(pctl::NEWNS, &[]).map_err(|e| err("pctl NEWNS", e))?;
        for (fd, at) in [(bin, "/bin"), (lib, "/lib")] {
            purgatorio::mount(fd.as_raw_fd(), -1, at, MREPL | MNOAUTH, "")
                .map_err(|e| err(&format!("mount {at}"), e))?;
        }
        for f in DEVS {
            purgatorio::bind(&format!("#c/{f}"), &format!("/dev/{f}"), MREPL)
                .map_err(|e| err(&format!("bind /dev/{f}"), e))?;
        }
        for (new, old, flags) in [
            ("#c/null", "/dev/cons", MREPL),
            ("#e", "/env", MREPL | MCREATE),
            ("#p", "/prog", MREPL),
            ("#d", "/fd", MREPL),
        ] {
            purgatorio::bind(new, old, flags).map_err(|e| err(&format!("bind {old}"), e))?;
        }
        s.helper("/bin/memfs.wasm", &["memfs", HOME], null(OREAD)?)?;
        purgatorio::chdir("/").map_err(|e| err("/", e))?;
        // What sh's profile would set. The environment is a copy, so the
        // server's own variables are not changed.
        for (name, file) in [("user", "/dev/user"), ("sysname", "/dev/sysname")] {
            if let Ok(v) = std::fs::read_to_string(file) {
                let _ = purgatorio::env::set(name, v.trim());
            }
        }
        let _ = purgatorio::env::set("home", HOME);
        if let Some(text) = policy {
            make_mount_points(text);
            // nsbuild reads the file as its standard input; the file
            // itself is in the server's namespace, not this one.
            s.helper(
                "/bin/nsbuild.wasm",
                &["nsbuild", "/fd/0"],
                feed(text).map_err(|e| format!("namespace file: {e}"))?,
            )
            .map_err(|e| format!("namespace file: {e}"))?;
        }
        purgatorio::pctl(pctl::NODEVS, &[]).map_err(|e| err("pctl NODEVS", e))?;
        let _ = purgatorio::chdir(HOME);
        Ok(s)
    }

    fn send(&mut self, v: Value) {
        let mut line = v.encode();
        line.push('\n');
        if let Some(out) = self.out.as_mut() {
            // A front end that has gone shows as the end of the input.
            let _ = out.write_all(line.as_bytes());
        }
    }

    fn reply(&mut self, tag: Value, outcome: Result<Value, String>, group: Option<u32>) {
        let mut m = vec![(String::from("tag"), tag)];
        match outcome {
            Ok(v) => m.push((String::from("result"), v)),
            Err(e) => m.push((String::from("error"), Value::Str(e))),
        }
        if let Some(g) = group {
            m.push((String::from("group"), Value::from(g)));
        }
        self.send(Value::Object(m));
    }

    /// Serves requests from the front end until it closes the pipe.
    pub fn serve(&mut self) -> Result<(), Error> {
        // SAFETY: fds 0 and 1 are this prog's pipes to the front end and
        // nothing else uses them.
        let (mut input, output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
        self.out = Some(output);
        self.send(obj([("ready", true.into())]));
        let mut acc = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let mut fds = vec![input.as_raw_fd(), self.wait.as_raw_fd()];
            let mut slots = Vec::new();
            for (p, proc) in self.procs.iter().enumerate() {
                for (k, s) in proc.streams.iter().enumerate() {
                    if let Some(fd) = &s.fd {
                        fds.push(fd.as_raw_fd());
                        slots.push((p, k));
                    }
                }
            }
            let due = self
                .procs
                .iter()
                .filter_map(Proc::deadline)
                .chain(self.parked.iter().map(|p| p.deadline))
                .min();
            match wait_readable(&fds, ms_until(due)) {
                Ok(Some(0)) => {
                    let n = input.read(&mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    self.send(obj([("ack", n.into())]));
                    acc.extend_from_slice(&buf[..n]);
                    while let Some(i) = acc.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = acc.drain(..=i).collect();
                        self.enqueue(&String::from_utf8_lossy(&line));
                    }
                }
                Ok(Some(1)) => self.reaped(),
                Ok(Some(i)) => {
                    let (p, k) = slots[i - 2];
                    self.procs[p].streams[k].fill();
                }
                Ok(None) => {}
                // Only a kill of this prog interrupts the wait. Leave at
                // once, so the front end sees the session end.
                Err(e) => purgatorio::exits(&format!("killed: {e}")),
            }
            self.tick();
        }
    }

    fn enqueue(&mut self, line: &str) {
        let Ok(req) = json::parse(line) else { return };
        let tag = req.get("tag").cloned().unwrap_or(Value::Null);
        let name = String::from(req.get("name").and_then(Value::as_str).unwrap_or(""));
        let args = req
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Vec::new()));
        self.queue.push_back(Request { tag, name, args });
    }

    /// Reads one exit message.
    fn reaped(&mut self) {
        let mut buf = [0u8; 4096];
        let n = match self.wait.read(&mut buf) {
            Ok(n) => n,
            Err(e) => purgatorio::exits(&format!("killed: {}", Error::from(e))),
        };
        let fields = purgatorio::unquote_fields(&String::from_utf8_lossy(&buf[..n]));
        let [p, _, status] = fields.as_slice() else {
            return;
        };
        let Ok(pid) = p.parse::<u32>() else { return };
        if let Some(proc) = self.procs.iter_mut().find(|x| x.pid == pid) {
            if proc.status.is_none() {
                proc.status = Some(status.clone());
                proc.exited_at = Some(Instant::now());
            }
        } else if let Some(k) = self.feeders.iter().position(|f| f.0 == pid) {
            let (_, id) = self.feeders.remove(k);
            if let Some(j) = self.job_mut(id) {
                j.feeding = false;
            }
            self.pump(id);
        }
    }

    /// Does what is due: kills commands past their timeout, answers the
    /// ones that are over and the job reads that can be, and begins
    /// queued requests.
    fn tick(&mut self) {
        let now = Instant::now();
        for p in &mut self.procs {
            if let Kind::Call { deadline, .. } = p.kind
                && p.status.is_none()
                && now >= deadline
            {
                p.kill(now);
            }
        }
        let mut i = 0;
        while i < self.procs.len() {
            let p = &mut self.procs[i];
            if !p.settled(now) {
                i += 1;
                continue;
            }
            match &mut p.kind {
                Kind::Job(j) => {
                    if !j.done {
                        j.done = true;
                        for s in &mut p.streams {
                            s.fd = None;
                        }
                        p.status.get_or_insert_with(|| String::from("killed"));
                    }
                    i += 1;
                }
                Kind::Call { .. } => {
                    let p = self.procs.remove(i);
                    self.finish_call(p);
                }
            }
        }
        let mut k = 0;
        while k < self.parked.len() {
            let w = &self.parked[k];
            let ready = match self.job(w.job) {
                Some(p) => {
                    job_done(p) || (0..2).any(|s| p.streams[s].end() > from_of(p, s, w.from[s]))
                }
                None => true,
            };
            if ready || now >= w.deadline {
                let w = self.parked.remove(k);
                let r = self.read_job(w.job, w.from, w.max);
                self.reply(w.tag, r, None);
            } else {
                k += 1;
            }
        }
        // Keep the newest exited jobs.
        let done: Vec<usize> = (0..self.procs.len())
            .filter(|&i| job_done(&self.procs[i]))
            .collect();
        if done.len() > MAX_JOBS {
            for &i in done[..done.len() - MAX_JOBS].iter().rev() {
                self.procs.remove(i);
            }
        }
        while !self.busy
            && let Some(r) = self.queue.pop_front()
        {
            self.begin(r);
        }
    }

    fn finish_call(&mut self, mut p: Proc) {
        p.wait_gone();
        let (tag, mount) = match &p.kind {
            Kind::Call { tag, mount, .. } => (tag.clone(), *mount),
            Kind::Job(_) => return,
        };
        self.busy = false;
        let killed = p.killed_at.is_some();
        let status = p.status.take().unwrap_or_else(|| String::from("killed"));
        let [out, errs] = &mut p.streams;
        let outcome = if mount {
            if status.is_empty() && !killed {
                Ok(obj([("ok", true.into())]))
            } else if killed {
                Err(String::from("mount: timed out"))
            } else {
                match errs.text().trim() {
                    "" => Err(format!("mount: {status}")),
                    t => Err(String::from(t)),
                }
            }
        } else {
            Ok(obj([
                ("stdout", out.text().into()),
                ("stderr", errs.text().into()),
                ("ok", (status.is_empty() && !killed).into()),
                ("status", status.into()),
                ("timed_out", killed.into()),
                ("stdout_truncated", out.truncated.into()),
                ("stderr_truncated", errs.truncated.into()),
            ]))
        };
        self.reply(tag, outcome, Some(p.pid));
    }

    /// Begins a request.
    fn begin(&mut self, r: Request) {
        self.send(obj([("start", r.tag.clone())]));
        // Tools name files from /tmp, whatever a command did to the
        // working directory the progs of a namespace share.
        let _ = purgatorio::chdir(HOME).or_else(|_| purgatorio::chdir("/"));
        let args = &r.args;
        let outcome = match r.name.as_str() {
            "run" | "mount" => match self.start_call(&r) {
                Ok(()) => {
                    self.busy = true;
                    return;
                }
                Err(e) => Err(e),
            },
            "job_start" => match self.job_start(args) {
                Ok((v, pid)) => {
                    self.reply(r.tag, Ok(v), Some(pid));
                    return;
                }
                Err(e) => Err(e),
            },
            "job_read" => {
                let id = int(args, "id") as u32;
                let from = [opt_int(args, "offset"), opt_int(args, "stderr_offset")];
                let max = opt_int(args, "max_bytes").unwrap_or(DEFAULT_OUTPUT as u64) as usize;
                let wait = opt_int(args, "wait_ms").unwrap_or(0);
                match self.job(id) {
                    Some(p)
                        if wait > 0
                            && !job_done(p)
                            && (0..2).all(|s| p.streams[s].end() <= from_of(p, s, from[s])) =>
                    {
                        self.parked.push(Parked {
                            tag: r.tag,
                            job: id,
                            from,
                            max,
                            deadline: Instant::now() + Duration::from_millis(wait),
                        });
                        return;
                    }
                    _ => self.read_job(id, from, max),
                }
            }
            "job_write" => self.job_write(args),
            "job_kill" => self.job_kill(args),
            "job_list" => Ok(self.job_list()),
            name => call(name, args),
        };
        self.reply(r.tag, outcome, None);
    }

    /// Starts the program of a `run` or a `mount`.
    fn start_call(&mut self, r: &Request) -> Result<(), String> {
        let args = &r.args;
        let s = |k: &str| args.get(k).and_then(Value::as_str);
        let (argv, cmd, stdin, timeout, cap) = if r.name == "mount" {
            let mut argv = vec![String::from("mount")];
            let mut flags = String::from("-");
            flags.push(match s("order") {
                Some("before") => 'b',
                Some("after") => 'a',
                _ => 'r',
            });
            if args.get("create").and_then(Value::as_bool) == Some(true) {
                flags.push('c');
            }
            if args.get("authenticate").and_then(Value::as_bool) == Some(false) {
                flags.push('A');
            }
            argv.push(flags);
            argv.push(String::from(s("source").unwrap_or("")));
            argv.push(String::from(s("to").unwrap_or("")));
            if let Some(spec) = s("spec") {
                argv.push(String::from(spec));
            }
            let timeout = opt_int(args, "timeout_ms").unwrap_or(MOUNT_TIMEOUT_MS as u64);
            (argv, "/bin/mount.wasm", null(OREAD)?, timeout, 64 * 1024)
        } else {
            let command = s("command").unwrap_or("");
            let argv = vec![
                String::from("sh"),
                String::from("-c"),
                String::from(command),
            ];
            let stdin = match s("stdin") {
                Some(text) if !text.is_empty() => feed(text)?,
                _ => null(OREAD)?,
            };
            let timeout = opt_int(args, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS as u64);
            let cap = opt_int(args, "max_output_bytes").unwrap_or(DEFAULT_OUTPUT as u64);
            if let Some(cwd) = s("cwd") {
                purgatorio::chdir(cwd).map_err(|e| err(cwd, e))?;
            }
            (argv, SH, stdin, timeout, cap as usize)
        };
        let kind = Kind::Call {
            tag: r.tag.clone(),
            deadline: Instant::now() + Duration::from_millis(timeout.max(1)),
            mount: r.name == "mount",
        };
        let p = Proc::start(cmd, &argv, stdin, cap, kind)?;
        self.procs.push(p);
        Ok(())
    }

    fn job(&self, id: u32) -> Option<&Proc> {
        self.procs
            .iter()
            .find(|p| matches!(&p.kind, Kind::Job(j) if j.id == id))
    }

    fn job_mut(&mut self, id: u32) -> Option<&mut Job> {
        self.procs.iter_mut().find_map(|p| match &mut p.kind {
            Kind::Job(j) if j.id == id => Some(j),
            _ => None,
        })
    }

    fn job_start(&mut self, args: &Value) -> Result<(Value, u32), String> {
        let running = self
            .procs
            .iter()
            .filter(|p| is_job(p) && !job_done(p))
            .count();
        if running >= MAX_JOBS {
            return Err(format!(
                "{MAX_JOBS} jobs are running; kill one with job_kill first"
            ));
        }
        let (cmd, argv) = match (args.get("command"), args.get("argv")) {
            (Some(Value::Str(c)), _) => (
                String::from(SH),
                vec![String::from("sh"), String::from("-c"), c.clone()],
            ),
            (_, Some(Value::Array(a))) => {
                let argv: Vec<String> = a
                    .iter()
                    .map(|v| String::from(v.as_str().unwrap_or("")))
                    .collect();
                (argv[0].clone(), argv)
            }
            _ => {
                return Err(String::from(
                    "arguments.command or arguments.argv is required",
                ));
            }
        };
        if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
            purgatorio::chdir(cwd).map_err(|e| err(cwd, e))?;
        }
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => String::from(c),
            None => argv.join(" "),
        };
        let [in_r, in_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let id = self.next_job;
        let job = Job {
            id,
            command,
            stdin: Some(File::from(in_w)),
            queue: VecDeque::new(),
            feeding: false,
            eof: false,
            cursor: [0, 0],
            done: false,
        };
        let p = Proc::start(&cmd, &argv, in_r, JOB_BUFFER, Kind::Job(job))?;
        self.next_job += 1;
        let pid = p.pid;
        self.procs.push(p);
        if let Some(text) = args.get("stdin").and_then(Value::as_str)
            && !text.is_empty()
        {
            if let Some(j) = self.job_mut(id) {
                j.queue.push_back(text.as_bytes().to_vec());
            }
            self.pump(id);
        }
        Ok((obj([("id", id.into()), ("pid", pid.into())]), pid))
    }

    /// Hands a job's queued input to a feeder, one at a time so the order
    /// holds, and closes the input once all is written and EOF was asked.
    fn pump(&mut self, id: u32) {
        let Some(j) = self.job_mut(id) else { return };
        if j.feeding {
            return;
        }
        let Some(fd) = j.stdin.as_ref().map(AsRawFd::as_raw_fd) else {
            j.queue.clear();
            return;
        };
        match j.queue.pop_front() {
            Some(data) => match start_feeder(&data, fd) {
                Ok(pid) => {
                    j.feeding = true;
                    self.feeders.push((pid, id));
                }
                // The job's input is gone; what is queued cannot be written.
                Err(_) => {
                    j.queue.clear();
                    j.stdin = None;
                }
            },
            None if j.eof => j.stdin = None,
            None => {}
        }
    }

    fn read_job(&mut self, id: u32, from: [Option<u64>; 2], max: usize) -> Result<Value, String> {
        let p = self
            .procs
            .iter_mut()
            .find(|p| matches!(&p.kind, Kind::Job(j) if j.id == id))
            .ok_or_else(|| format!("job {id}: no such job"))?;
        let starts = [from_of(p, 0, from[0]), from_of(p, 1, from[1])];
        let lost = [0, 1].map(|s| p.streams[s].start.saturating_sub(starts[s]));
        let (out, o0, o1) = p.streams[0].read(starts[0], max);
        let (errs, e0, e1) = p.streams[1].read(starts[1], max);
        let done = job_done(p);
        let status = if done {
            p.status.clone().unwrap_or_default()
        } else {
            String::new()
        };
        if let Kind::Job(j) = &mut p.kind {
            j.cursor = [o1, e1];
        }
        Ok(obj([
            ("stdout", out.into()),
            ("stderr", errs.into()),
            ("offset", o0.into()),
            ("next_offset", o1.into()),
            ("stderr_offset", e0.into()),
            ("next_stderr_offset", e1.into()),
            ("lost", lost[0].into()),
            ("stderr_lost", lost[1].into()),
            ("running", (!done).into()),
            ("ok", (done && status.is_empty()).into()),
            ("status", status.into()),
        ]))
    }

    fn job_write(&mut self, args: &Value) -> Result<Value, String> {
        let id = int(args, "id") as u32;
        let text = args.get("data").and_then(Value::as_str).unwrap_or("");
        let data = match args.get("encoding").and_then(Value::as_str) {
            Some("base64") => base64_decode(text).map_err(|e| format!("arguments.data: {e}"))?,
            _ => text.as_bytes().to_vec(),
        };
        let exited = self
            .job(id)
            .ok_or_else(|| format!("job {id}: no such job"))?
            .status
            .is_some();
        let j = self
            .job_mut(id)
            .ok_or_else(|| format!("job {id}: no such job"))?;
        if exited {
            return Err(format!("job {id} has exited"));
        }
        if j.eof || j.stdin.is_none() {
            return Err(format!("job {id}: its input is closed"));
        }
        let queued = data.len();
        if !data.is_empty() {
            j.queue.push_back(data);
        }
        if args.get("eof").and_then(Value::as_bool) == Some(true) {
            j.eof = true;
        }
        let eof = j.eof;
        self.pump(id);
        Ok(obj([("queued", queued.into()), ("eof", eof.into())]))
    }

    fn job_kill(&mut self, args: &Value) -> Result<Value, String> {
        let id = int(args, "id") as u32;
        let p = self
            .procs
            .iter_mut()
            .find(|p| matches!(&p.kind, Kind::Job(j) if j.id == id))
            .ok_or_else(|| format!("job {id}: no such job"))?;
        let killed = p.status.is_none() && p.killed_at.is_none();
        if killed {
            p.kill(Instant::now());
        }
        Ok(obj([("killed", killed.into())]))
    }

    fn job_list(&self) -> Value {
        let jobs = self
            .procs
            .iter()
            .filter_map(|p| match &p.kind {
                Kind::Job(j) => Some(obj([
                    ("id", j.id.into()),
                    ("pid", p.pid.into()),
                    ("command", j.command.as_str().into()),
                    ("running", (!j.done).into()),
                    (
                        "status",
                        (if j.done {
                            p.status.clone().unwrap_or_default()
                        } else {
                            String::new()
                        })
                        .into(),
                    ),
                    ("stdout_bytes", p.streams[0].end().into()),
                    ("stderr_bytes", p.streams[1].end().into()),
                ])),
                Kind::Call { .. } => None,
            })
            .collect();
        obj([("jobs", Value::Array(jobs))])
    }
}

fn is_job(p: &Proc) -> bool {
    matches!(p.kind, Kind::Job(_))
}

fn job_done(p: &Proc) -> bool {
    matches!(&p.kind, Kind::Job(j) if j.done)
}

/// Where a read of stream `s` starts: the offset asked for, or the cursor.
fn from_of(p: &Proc, s: usize, asked: Option<u64>) -> u64 {
    match (&p.kind, asked) {
        (_, Some(o)) => o,
        (Kind::Job(j), None) => j.cursor[s],
        _ => 0,
    }
}

fn int(args: &Value, key: &str) -> i64 {
    args.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn opt_int(args: &Value, key: &str) -> Option<u64> {
    args.get(key)
        .and_then(Value::as_i64)
        .map(|n| n.max(0) as u64)
}

/// The requests that finish at once.
fn call(name: &str, args: &Value) -> Result<Value, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str);
    let path = || {
        s("path")
            .filter(|p| !p.is_empty())
            .ok_or_else(|| String::from("arguments.path must not be empty"))
    };
    match name {
        "read_file" => files::read_file(path()?, args),
        "write_file" => files::write_file(path()?, args),
        "list_dir" => files::list_dir(path()?),
        "stat" => files::stat(path()?),
        "remove" => {
            let path = path()?;
            let recursive = args.get("recursive").and_then(Value::as_bool) == Some(true);
            let mut removed = 0u64;
            files::remove(path, recursive, &mut removed)?;
            Ok(obj([("removed", removed.into())]))
        }
        "ns" => {
            let me = purgatorio::pid();
            std::fs::read_to_string(format!("/prog/{me}/ns"))
                .map(|ns| obj([("ns", ns.into())]))
                .map_err(|e| err("ns", Error::from(e)))
        }
        "bind" => {
            let (from, to) = (s("from").unwrap_or(""), s("to").unwrap_or(""));
            let flags = order(s("order")) | create(args);
            purgatorio::bind(from, to, flags)
                .map(|()| obj([("ok", true.into())]))
                .map_err(|e| format!("bind: cannot bind {from} on {to}: {e}"))
        }
        "unmount" => {
            let to = s("to").unwrap_or("");
            purgatorio::unmount(s("from"), to)
                .map(|()| obj([("ok", true.into())]))
                .map_err(|e| format!("unmount: {to}: {e}"))
        }
        "resources/list" => files::list_resources(HOME),
        "resources/read" => files::read_resource(path()?),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn order(o: Option<&str>) -> u32 {
    match o {
        Some("before") => MBEFORE,
        Some("after") => MAFTER,
        _ => MREPL,
    }
}

fn create(args: &Value) -> u32 {
    if args.get("create").and_then(Value::as_bool) == Some(true) {
        MCREATE
    } else {
        0
    }
}

/// Makes the mount points a namespace file names that the new root lacks,
/// since namespace(6) has no way to make one and the root starts with only
/// a few. Lines that name variables are left to nsbuild.
fn make_mount_points(text: &str) {
    for line in text.lines() {
        let words = purgatorio::unquote_fields(line);
        let Some(verb) = words.first() else { continue };
        let mut args = Vec::new();
        let mut it = words[1..].iter();
        while let Some(w) = it.next() {
            if args.is_empty() && w.starts_with('-') && w != "-" {
                if w == "--" {
                    args.extend(it.by_ref().cloned());
                    break;
                }
                if verb == "mount" && (w.contains('k') || w.contains('C')) {
                    it.next();
                }
                continue;
            }
            args.push(w.clone());
        }
        let target = match verb.as_str() {
            "bind" | "mount" => args.get(1),
            "import" if args.len() >= 2 => args.last(),
            _ => None,
        };
        let Some(target) = target.filter(|t| t.starts_with('/') && !t.contains('$')) else {
            continue;
        };
        if purgatorio::stat(target).is_ok() {
            continue;
        }
        let mut at = String::new();
        for part in target.split('/').filter(|p| !p.is_empty()) {
            at.push('/');
            at.push_str(part);
            if purgatorio::stat(&at).is_err() {
                let _ = purgatorio::create(&at, OREAD, DMDIR | 0o777);
            }
        }
    }
}
