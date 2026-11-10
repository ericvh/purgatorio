//! The session prog: `mcpd --session [nsfile]`.
//!
//! The front end starts one of these per session with a forked namespace
//! and environment and a new process group. It builds the sandbox, gives up
//! the right to attach devices, and then serves tool calls: one JSON line
//! in on fd 0, one line out on fd 1. Everything a tool touches is named in
//! the sandbox, because this prog has no other namespace.
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
//! 5. The lines of the namespace file, if one was given, run as sh commands.
//!    This is where a policy grants more: `bind -c '#U/work' /n/work`,
//!    `bind '#I' /net`.
//! 6. `pctl(NODEVS)`: from here on neither this prog nor anything it starts
//!    can name a device with `#`, except the ones Inferno always allows
//!    (`#e`, `#s`, `#|`, `#M`, `#¤`).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::mount::{MCREATE, MNOAUTH, MREPL};
use purgatorio::abi::pctl;
use purgatorio::{DMAPPEND, DMDIR, Dir, Error};

use crate::json::{self, Value, base64_decode, base64_encode, obj};
use crate::tooldefs::{DEFAULT_OUTPUT, DEFAULT_READ, DEFAULT_TIMEOUT_MS, MAX_ENTRIES};

const SELF: &str = "/bin/mcpd.wasm";
const SH: &str = "/bin/sh.wasm";
/// Where relative names start and where `run` works by default.
const HOME: &str = "/tmp";
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
/// How long a killed command gets to die before `run` gives up on it.
const KILL_GRACE: Duration = Duration::from_millis(3000);
/// How long `run` keeps collecting output that is already on its way after
/// the command has exited.
const DRAIN: Duration = Duration::from_millis(200);

fn err(what: &str, e: impl std::fmt::Display) -> String {
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

pub struct Session {
    /// Open on this prog's `/prog/n/wait`.
    wait: File,
}

impl Session {
    /// Reads exit messages until the one for `pid`; returns its status.
    fn reap(&mut self, pid: u32) -> Result<String, String> {
        loop {
            if let Some(status) = self.reap_one(pid)? {
                return Ok(status);
            }
        }
    }

    /// Reads one exit message. `Some(status)` if it was `pid`'s.
    fn reap_one(&mut self, pid: u32) -> Result<Option<String>, String> {
        let mut buf = [0u8; 4096];
        let n = self
            .wait
            .read(&mut buf)
            .map_err(|e| err("wait", Error::from(e)))?;
        let fields = purgatorio::unquote_fields(&String::from_utf8_lossy(&buf[..n]));
        match fields.as_slice() {
            [p, _, status] if p.parse() == Ok(pid) => Ok(Some(status.clone())),
            _ => Ok(None),
        }
    }

    /// Runs a helper command to completion with no input and its output on
    /// this prog's fd 2 (the log).
    fn helper(&mut self, cmd: &str, argv: &[&str]) -> Result<(), String> {
        let argv: Vec<String> = argv.iter().map(|a| String::from(*a)).collect();
        let pid = {
            let zero = null(OREAD)?;
            purgatorio::spawn(
                cmd,
                &argv,
                pctl::NEWFD,
                &[(zero.as_raw_fd(), 0), (2, 1), (2, 2)],
            )
            .map_err(|e| err(&argv.join(" "), e))?
        };
        let status = self.reap(pid)?;
        if status.is_empty() {
            Ok(())
        } else {
            Err(err(&argv.join(" "), status))
        }
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
        };
        // Any directory will do to hold the new root while it is built; the
        // namespace it is in is about to be thrown away.
        let stage = ["/tmp", "/mnt", "/n", "/chan"]
            .into_iter()
            .find(|d| purgatorio::stat(d).is_ok_and(|d| d.mode & DMDIR != 0))
            .ok_or("no directory to build the session root on")?;
        s.helper("/bin/memfs.wasm", &["memfs", stage])?;
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
        s.helper("/bin/memfs.wasm", &["memfs", HOME])?;
        purgatorio::chdir("/").map_err(|e| err("/", e))?;
        // What sh's profile would set. The environment is a copy, so the
        // server's own variables are not changed.
        for (name, file) in [("user", "/dev/user"), ("sysname", "/dev/sysname")] {
            if let Ok(v) = std::fs::read_to_string(file) {
                let _ = purgatorio::env::set(name, v.trim());
            }
        }
        let _ = purgatorio::env::set("home", HOME);
        for line in policy.unwrap_or("").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            s.helper(SH, &["sh", "-c", line])
                .map_err(|e| format!("namespace file: {e}"))?;
        }
        purgatorio::pctl(pctl::NODEVS, &[]).map_err(|e| err("pctl NODEVS", e))?;
        let _ = purgatorio::chdir(HOME);
        Ok(s)
    }

    /// Serves tool calls from the front end until it closes the pipe.
    pub fn serve(&mut self) -> Result<(), Error> {
        // SAFETY: fds 0 and 1 are this prog's pipes to the front end and
        // nothing else uses them.
        let (input, mut output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
        let mut input = BufReader::new(input);
        output.write_all(b"{\"ready\":true}\n")?;
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let mut group = None;
            let outcome = json::parse(&line).and_then(|req| {
                let name = req.get("name").and_then(Value::as_str).unwrap_or("");
                let empty = Value::Object(Vec::new());
                let args = req.get("arguments").unwrap_or(&empty);
                self.call(name, args, &mut group)
            });
            let mut reply = match outcome {
                Ok(v) => vec![(String::from("result"), v)],
                Err(e) => vec![(String::from("error"), Value::Str(e))],
            };
            if let Some(g) = group {
                reply.push((String::from("group"), Value::from(g)));
            }
            let mut text = Value::Object(reply).encode();
            text.push('\n');
            output.write_all(text.as_bytes())?;
        }
    }

    fn call(&mut self, name: &str, args: &Value, group: &mut Option<u32>) -> Result<Value, String> {
        // Tools name files from /tmp, whatever a command did to the
        // working directory the progs of a namespace share.
        let _ = purgatorio::chdir(HOME).or_else(|_| purgatorio::chdir("/"));
        let path = || {
            args.get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| String::from("arguments.path must not be empty"))
        };
        match name {
            "run" => self.run(args, group),
            "read_file" => read_file(path()?, args),
            "write_file" => write_file(path()?, args),
            "list_dir" => list_dir(path()?),
            "stat" => {
                let path = path()?;
                let d = purgatorio::stat(path).map_err(|e| err(path, e))?;
                let full = purgatorio::open(path, OREAD)
                    .ok()
                    .and_then(|fd| purgatorio::fd2path(fd.as_raw_fd()).ok())
                    .unwrap_or_else(|| String::from(path));
                let mut m = entry(&d);
                m.extend([
                    (String::from("path"), Value::from(full)),
                    (String::from("atime"), Value::from(d.atime)),
                    (String::from("uid"), Value::from(d.uid)),
                    (String::from("gid"), Value::from(d.gid)),
                    (String::from("muid"), Value::from(d.muid)),
                ]);
                Ok(Value::Object(m))
            }
            "remove" => {
                let path = path()?;
                let recursive = args.get("recursive").and_then(Value::as_bool) == Some(true);
                let mut removed = 0u64;
                remove(path, recursive, &mut removed)?;
                Ok(obj([("removed", removed.into())]))
            }
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn run(&mut self, args: &Value, group: &mut Option<u32>) -> Result<Value, String> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let timeout = Duration::from_millis(timeout.max(1) as u64);
        let cap = args
            .get("max_output_bytes")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_OUTPUT)
            .max(0) as usize;
        if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
            purgatorio::chdir(cwd).map_err(|e| err(cwd, e))?;
        }
        let [out_r, out_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let [err_r, err_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let stdin = match args.get("stdin").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => feed(text)?,
            _ => null(OREAD)?,
        };
        let argv = ["sh", "-c", command].map(String::from);
        // NEWPGRP: the command and what it starts are one group, killed
        // together on a timeout. The namespace and environment are shared
        // with this prog, so binds and variables outlive the command.
        let pid = purgatorio::spawn(
            SH,
            &argv,
            pctl::NEWFD | pctl::NEWPGRP,
            &[
                (stdin.as_raw_fd(), 0),
                (out_w.as_raw_fd(), 1),
                (err_w.as_raw_fd(), 2),
            ],
        )
        .map_err(|e| err("sh", e))?;
        drop((stdin, out_w, err_w));
        *group = Some(pid);
        // Opened now, so that the group can be killed even if the command
        // takes /prog out of the namespace.
        let ctl = purgatorio::open(&format!("/prog/{pid}/ctl"), OWRITE).ok();

        let mut streams = [Capture::new(out_r, cap), Capture::new(err_r, cap)];
        let start = Instant::now();
        let mut status: Option<String> = None;
        let mut exited_at = None;
        let mut killed_at: Option<Instant> = None;
        loop {
            let open: Vec<usize> = (0..2).filter(|&i| streams[i].fd.is_some()).collect();
            if status.is_some() && open.is_empty() {
                break;
            }
            let mut fds: Vec<i32> = open.iter().map(|&i| streams[i].raw()).collect();
            if status.is_none() {
                fds.push(self.wait.as_raw_fd());
            }
            let limit = match (exited_at, killed_at) {
                (Some(at), _) => DRAIN.saturating_sub(Instant::now() - at),
                (None, Some(at)) => KILL_GRACE.saturating_sub(Instant::now() - at),
                (None, None) => timeout.saturating_sub(start.elapsed()),
            };
            let limit = i32::try_from(limit.as_millis()).unwrap_or(i32::MAX);
            match wait_readable(&fds, limit) {
                Ok(Some(i)) if i < open.len() => streams[open[i]].fill(),
                Ok(Some(_)) => {
                    if let Some(s) = self.reap_one(pid)? {
                        status = Some(s);
                        exited_at = Some(Instant::now());
                    }
                }
                // Nothing more within the limit.
                Ok(None) if exited_at.is_some() || killed_at.is_some() => break,
                Ok(None) => {
                    killed_at = Some(Instant::now());
                    kill_group(pid, ctl.as_ref());
                }
                // Only a kill of this prog interrupts the wait. Leave at once,
                // so the front end sees the session end and starts another.
                Err(e) => purgatorio::exits(&format!("killed: {e}")),
            }
        }
        // A killed prog leaves at its next system call; do not answer
        // until the group is gone, so the caller sees it dead.
        if let Some(at) = killed_at {
            while at.elapsed() < KILL_GRACE && progs("/prog").iter().any(|p| p.group == pid) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let status = status.unwrap_or_else(|| String::from("killed"));
        let [out, errs] = streams;
        Ok(obj([
            ("stdout", out.text().into()),
            ("stderr", errs.text().into()),
            ("ok", (status.is_empty() && killed_at.is_none()).into()),
            ("status", status.into()),
            ("timed_out", killed_at.is_some().into()),
            ("stdout_truncated", out.truncated.into()),
            ("stderr_truncated", errs.truncated.into()),
        ]))
    }
}

/// One of a command's output pipes, kept up to a cap.
struct Capture {
    fd: Option<File>,
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl Capture {
    fn new(fd: OwnedFd, cap: usize) -> Capture {
        Capture {
            fd: Some(File::from(fd)),
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn raw(&self) -> i32 {
        self.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    /// Reads what is ready. Output past the cap is read and dropped, so the
    /// command is not blocked by a full pipe.
    fn fill(&mut self) {
        let mut buf = [0u8; 8192];
        let n = match self.fd.as_mut().map(|f| f.read(&mut buf)) {
            Some(Ok(n)) if n > 0 => n,
            _ => {
                self.fd = None;
                return;
            }
        };
        let keep = n.min(self.cap - self.data.len());
        self.data.extend_from_slice(&buf[..keep]);
        self.truncated |= keep < n;
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// Kills every prog in the process group led by `pid`.
fn kill_group(pid: u32, ctl: Option<&OwnedFd>) {
    if let Some(ctl) = ctl
        && let Ok(fd) = purgatorio::dup(ctl.as_raw_fd(), -1)
    {
        // SAFETY: `dup` returned a new fd that nothing else owns.
        let mut f = unsafe { File::from_raw_fd(fd) };
        if f.write_all(b"killgrp").is_ok() {
            return;
        }
    }
    // The leader has gone; find the members that are left.
    for p in progs("/prog") {
        if p.group == pid {
            let _ = std::fs::write(format!("/prog/{}/ctl", p.pid), "kill");
        }
    }
}

pub struct ProgInfo {
    pub pid: u32,
    pub ppid: u32,
    pub group: u32,
}

/// The progs listed in `dir` (a `#p`), from their `status` files.
pub fn progs(dir: &str) -> Vec<ProgInfo> {
    let Ok(fd) = purgatorio::open(dir, OREAD) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(batch) = purgatorio::dirread(fd.as_raw_fd()) {
        if batch.is_empty() {
            break;
        }
        for d in batch {
            let Ok(text) = std::fs::read_to_string(format!("{dir}/{}/status", d.name)) else {
                continue;
            };
            let f: Vec<&str> = text.split_whitespace().collect();
            if let [pid, ppid, group, ..] = f.as_slice()
                && let (Ok(pid), Ok(ppid), Ok(group)) = (pid.parse(), ppid.parse(), group.parse())
            {
                out.push(ProgInfo { pid, ppid, group });
            }
        }
    }
    out
}

/// Starts a prog that writes `text` to a pipe and returns the read end. A
/// separate prog does the writing because a pipe holds only so much: the
/// command may not read its input until it has written output this prog
/// must be free to collect.
fn feed(text: &str) -> Result<OwnedFd, String> {
    let [data_r, data_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
    let [in_r, in_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
    let argv = ["mcpd", "--feed"].map(String::from);
    purgatorio::spawn(
        SELF,
        &argv,
        pctl::NEWFD,
        &[(data_r.as_raw_fd(), 0), (in_w.as_raw_fd(), 1)],
    )
    .map_err(|e| err("stdin feeder", e))?;
    drop((data_r, in_w));
    // The feeder reads all of this before it writes any of it, so this
    // write cannot wait on the command.
    File::from(data_w)
        .write_all(text.as_bytes())
        .map_err(|e| err("stdin", Error::from(e)))?;
    Ok(in_r)
}

/// `mcpd --feed`: copy fd 0 to fd 1, reading all of it first.
pub fn feeder() {
    // SAFETY: fds 0 and 1 are the pipes `feed` gave this prog.
    let (mut input, mut output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let mut data = Vec::new();
    if input.read_to_end(&mut data).is_ok() {
        drop(input);
        let _ = output.write_all(&data);
    }
}

fn entry(d: &Dir) -> Vec<(String, Value)> {
    let dir = d.mode & DMDIR != 0;
    vec![
        (String::from("name"), Value::from(d.name.as_str())),
        (
            String::from("type"),
            Value::from(if dir { "dir" } else { "file" }),
        ),
        (String::from("size"), Value::from(d.length)),
        (
            String::from("mode"),
            Value::from(format!("{:o}", d.mode & 0o777)),
        ),
        (
            String::from("append_only"),
            Value::from(d.mode & DMAPPEND != 0),
        ),
        (String::from("mtime"), Value::from(d.mtime)),
    ]
}

fn read_dir(path: &str) -> Result<Vec<Dir>, String> {
    let d = purgatorio::stat(path).map_err(|e| err(path, e))?;
    if d.mode & DMDIR == 0 {
        return Err(err(path, "not a directory"));
    }
    let fd = purgatorio::open(path, OREAD).map_err(|e| err(path, e))?;
    let mut all = Vec::new();
    loop {
        let batch = purgatorio::dirread(fd.as_raw_fd()).map_err(|e| err(path, e))?;
        if batch.is_empty() {
            return Ok(all);
        }
        all.extend(batch);
    }
}

fn list_dir(path: &str) -> Result<Value, String> {
    let mut all = read_dir(path)?;
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let truncated = all.len() > MAX_ENTRIES;
    all.truncate(MAX_ENTRIES);
    Ok(obj([
        (
            "entries",
            Value::Array(all.iter().map(|d| Value::Object(entry(d))).collect()),
        ),
        ("truncated", truncated.into()),
    ]))
}

fn remove(path: &str, recursive: bool, removed: &mut u64) -> Result<(), String> {
    if recursive && purgatorio::stat(path).is_ok_and(|d| d.mode & DMDIR != 0) {
        for d in read_dir(path)? {
            remove(&format!("{path}/{}", d.name), true, removed)?;
        }
    }
    purgatorio::remove(path).map_err(|e| err(path, e))?;
    *removed += 1;
    Ok(())
}

fn read_file(path: &str, args: &Value) -> Result<Value, String> {
    let offset = args
        .get("offset")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u64;
    let length = args
        .get("length")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_READ)
        .max(0) as usize;
    let force = args.get("encoding").and_then(Value::as_str) == Some("base64");
    let io = |e: std::io::Error| err(path, Error::from(e));
    let fd = purgatorio::open(path, OREAD).map_err(|e| err(path, e))?;
    let size = purgatorio::fstat(fd.as_raw_fd())
        .map_err(|e| err(path, e))
        .and_then(|d| {
            if d.mode & DMDIR != 0 {
                Err(err(path, "file is a directory"))
            } else {
                Ok(d.length)
            }
        })?;
    let mut f = File::from(fd);
    if offset > 0 {
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
    }
    let mut buf = vec![0u8; length];
    let mut n = 0;
    let mut eof = false;
    while n < length {
        match f.read(&mut buf[n..]).map_err(io)? {
            0 => {
                eof = true;
                break;
            }
            k => n += k,
        }
    }
    buf.truncate(n);
    eof |= size > 0 && offset + n as u64 >= size;
    // A read cut in the middle of a character gives the whole characters;
    // the next read starts at the cut one.
    if !force
        && let Err(e) = std::str::from_utf8(&buf)
        && e.error_len().is_none()
        && e.valid_up_to() > 0
    {
        buf.truncate(e.valid_up_to());
        eof = false;
    }
    let bytes = buf.len();
    let (content, encoding) = match String::from_utf8(buf) {
        Ok(s) if !force => (s, "utf8"),
        Ok(s) => (base64_encode(s.as_bytes()), "base64"),
        Err(e) => (base64_encode(e.as_bytes()), "base64"),
    };
    Ok(obj([
        ("content", content.into()),
        ("encoding", encoding.into()),
        ("offset", offset.into()),
        ("bytes", bytes.into()),
        ("size", size.into()),
        ("eof", eof.into()),
    ]))
}

fn write_file(path: &str, args: &Value) -> Result<Value, String> {
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    let data = match args.get("encoding").and_then(Value::as_str) {
        Some("base64") => base64_decode(content).map_err(|e| format!("arguments.content: {e}"))?,
        _ => content.as_bytes().to_vec(),
    };
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("truncate");
    let exists = purgatorio::stat(path).is_ok();
    let io = |e: std::io::Error| err(path, Error::from(e));
    let mut f = if mode == "create" && exists {
        return Err(err(path, "file already exists"));
    } else if mode == "append" && exists {
        // Inferno has no append open mode; the offset is set by hand.
        let fd = purgatorio::open(path, OWRITE).map_err(|e| err(path, e))?;
        let mut f = File::from(fd);
        f.seek(SeekFrom::End(0)).map_err(io)?;
        f
    } else {
        File::from(purgatorio::create(path, OWRITE, 0o666).map_err(|e| err(path, e))?)
    };
    f.write_all(&data).map_err(io)?;
    let size = purgatorio::fstat(f.as_raw_fd()).map_or(0, |d| d.length);
    Ok(obj([("bytes", data.len().into()), ("size", size.into())]))
}
