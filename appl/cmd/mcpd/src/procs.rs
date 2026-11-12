//! Programs the session runs: commands for `run` and `mount`, which the
//! session waits for, and jobs, which it keeps. Each has pipes for its
//! output, read whenever they are ready, and its exit status from the
//! session prog's wait file. Also the small helper progs mcpd starts from
//! its own binary: the launcher, the stdin feeder and the input relay.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use purgatorio::Error;
use purgatorio::abi::mode::{OREAD, OWRITE};
use purgatorio::abi::pctl;

use crate::json::Value;
use crate::session::err;

const SELF: &str = "/bin/mcpd.wasm";
/// How long a killed command gets to die.
pub const KILL_GRACE: Duration = Duration::from_millis(3000);
/// How long output that is already on its way is still collected after a
/// program has exited. Programs it started may hold its pipes open.
pub const DRAIN: Duration = Duration::from_millis(200);
/// How the launcher's exit status starts when it could not start the
/// program: the rest is why.
pub const EXEC_FAILED: &str = "mcpd: cannot start: ";

/// One output pipe of a program and what has come out of it.
pub struct Stream {
    pub fd: Option<File>,
    /// Bytes from offset `start` on.
    pub data: VecDeque<u8>,
    pub start: u64,
    cap: usize,
    /// Keep the last `cap` bytes (a job) rather than the first (a call).
    ring: bool,
    pub truncated: bool,
}

impl Stream {
    pub fn new(fd: OwnedFd, cap: usize, ring: bool) -> Stream {
        Stream {
            fd: Some(File::from(fd)),
            data: VecDeque::new(),
            start: 0,
            cap,
            ring,
            truncated: false,
        }
    }

    /// The offset after the last byte that has come out.
    pub fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// Reads what is ready. Output past the cap is dropped, at the end for a
    /// call and at the start for a job, so the program is never held up by
    /// a full pipe.
    pub fn fill(&mut self) {
        let mut buf = [0u8; 8192];
        let n = match self.fd.as_mut().map(|f| f.read(&mut buf)) {
            Some(Ok(n)) if n > 0 => n,
            _ => {
                self.fd = None;
                return;
            }
        };
        if self.ring {
            self.data.extend(&buf[..n]);
            let over = self.data.len().saturating_sub(self.cap);
            if over > 0 {
                self.data.drain(..over);
                self.start += over as u64;
                self.truncated = true;
            }
        } else {
            let keep = n.min(self.cap - self.data.len());
            self.data.extend(&buf[..keep]);
            self.truncated |= keep < n;
        }
    }

    pub fn text(&mut self) -> String {
        String::from_utf8_lossy(self.data.make_contiguous()).into_owned()
    }

    /// Up to `max` bytes from offset `from` as text, and the offsets of the
    /// first byte returned and the one after the last. While more can come,
    /// a character cut at the end is left for the next read.
    pub fn read(&mut self, from: u64, max: usize) -> (String, u64, u64) {
        let from = from.clamp(self.start, self.end());
        let skip = (from - self.start) as usize;
        let open = self.fd.is_some();
        let data = self.data.make_contiguous();
        let mut piece = &data[skip..(skip + max).min(data.len())];
        if let Err(e) = std::str::from_utf8(piece)
            && e.error_len().is_none()
            && (open || piece.len() == max)
        {
            piece = &piece[..e.valid_up_to()];
        }
        let next = from + piece.len() as u64;
        (String::from_utf8_lossy(piece).into_owned(), from, next)
    }
}

/// What the session does with a program when it is over.
pub enum Kind {
    /// Answer request `tag`: a `run` or a `mount`. The program is killed
    /// at `deadline`.
    Call {
        tag: Value,
        deadline: Instant,
        mount: bool,
    },
    /// Answer request `tag` with what a file worker (`mcpd --file`)
    /// writes; it is killed at `deadline`.
    File { tag: Value, deadline: Instant },
    /// Keep it for the job tools.
    Job(Job),
}

impl Kind {
    /// When the program is killed, if it has a time limit.
    fn limit(&self) -> Option<Instant> {
        match self {
            Kind::Call { deadline, .. } | Kind::File { deadline, .. } => Some(*deadline),
            Kind::Job(j) => j.deadline,
        }
    }
}

pub struct Job {
    pub id: u32,
    pub command: String,
    /// The write end of the job's standard input, until it is closed.
    pub stdin: Option<File>,
    /// Data for the input, not yet given to a feeder.
    pub queue: VecDeque<Vec<u8>>,
    /// A feeder is writing to the input.
    pub feeding: bool,
    /// Close the input once the queue is written.
    pub eof: bool,
    /// Where the last job_read ended, for stdout and stderr.
    pub cursor: [u64; 2],
    /// Exited, and its output collected.
    pub done: bool,
    /// When it is killed, if it was given a `timeout_ms`.
    pub deadline: Option<Instant>,
    /// It was killed at its deadline.
    pub timed_out: bool,
    /// Why it failed, when it was a limit of the session.
    pub note: Option<String>,
}

pub struct Proc {
    pub pid: u32,
    /// Open on `/prog/<pid>/ctl`, so that the group can be killed even if
    /// a command takes /prog out of the namespace.
    pub ctl: Option<OwnedFd>,
    pub streams: [Stream; 2],
    pub status: Option<String>,
    pub exited_at: Option<Instant>,
    pub killed_at: Option<Instant>,
    pub kind: Kind,
}

impl Proc {
    /// Starts `argv` (`cmd` is the program, found as sh finds it) through
    /// the launcher, `mcpd --exec`, which changes to `cwd` (empty for
    /// /tmp) and starts the program there. The session prog itself then
    /// neither changes directory nor looks the program up, so a file
    /// server that hangs on either holds up only this call.
    pub fn launch(
        cwd: &str,
        cmd: &str,
        argv: &[String],
        stdin: OwnedFd,
        cap: usize,
        kind: Kind,
    ) -> Result<Proc, String> {
        let mut args = vec![
            String::from("mcpd"),
            String::from("--exec"),
            String::from(cwd),
            String::from(cmd),
        ];
        args.extend(argv.iter().cloned());
        Proc::start(SELF, &args, stdin, cap, kind)
    }

    /// Starts `argv` (`cmd` is the module) in a new process group, with
    /// `stdin` as its input and pipes for its output, capped at `cap`.
    pub fn start(
        cmd: &str,
        argv: &[String],
        stdin: OwnedFd,
        cap: usize,
        kind: Kind,
    ) -> Result<Proc, String> {
        let [out_r, out_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        let [err_r, err_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
        // NEWPGRP: the program and what it starts are one group, killed
        // together. The namespace and environment are shared with the
        // session prog, so binds and variables outlive the program.
        let pid = purgatorio::spawn(
            cmd,
            argv,
            pctl::NEWFD | pctl::NEWPGRP,
            &[
                (stdin.as_raw_fd(), 0),
                (out_w.as_raw_fd(), 1),
                (err_w.as_raw_fd(), 2),
            ],
        )
        .map_err(|e| err(&argv[0], e))?;
        drop((stdin, out_w, err_w));
        let ring = matches!(kind, Kind::Job(_) | Kind::File { .. });
        Ok(Proc {
            pid,
            ctl: purgatorio::open(&format!("/prog/{pid}/ctl"), OWRITE).ok(),
            streams: [Stream::new(out_r, cap, ring), Stream::new(err_r, cap, ring)],
            status: None,
            exited_at: None,
            killed_at: None,
            kind,
        })
    }

    /// Whether the program is a call that has run past its limit.
    pub fn overdue(&self, now: Instant) -> bool {
        self.status.is_none() && self.kind.limit().is_some_and(|d| now >= d)
    }

    pub fn kill(&mut self, now: Instant) {
        if self.killed_at.is_none() {
            self.killed_at = Some(now);
            kill_group(self.pid, self.ctl.as_ref());
        }
    }

    /// Whether the program is over: it has exited and its pipes are closed
    /// or have had [`DRAIN`] to empty, or it was killed [`KILL_GRACE`] ago.
    pub fn settled(&self, now: Instant) -> bool {
        let open = self.streams.iter().any(|s| s.fd.is_some());
        (self.status.is_some() && !open)
            || self.exited_at.is_some_and(|t| now >= t + DRAIN)
            || self.killed_at.is_some_and(|t| now >= t + KILL_GRACE)
    }

    /// When something is next due for this program, if anything is.
    pub fn deadline(&self) -> Option<Instant> {
        if let Kind::Job(j) = &self.kind
            && j.done
        {
            return None;
        }
        let mut due = [
            self.exited_at.map(|t| t + DRAIN),
            self.killed_at.map(|t| t + KILL_GRACE),
            None,
        ];
        if self.status.is_none() && self.killed_at.is_none() {
            due[2] = self.kind.limit();
        }
        due.into_iter().flatten().min()
    }

    /// Waits, a little, until no prog of the group is left, so that a
    /// caller told the program was killed finds it gone.
    pub fn wait_gone(&self) {
        if let Some(at) = self.killed_at {
            while at.elapsed() < KILL_GRACE && progs("/prog").iter().any(|p| p.group == self.pid) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Kills every prog in the process group led by `pid`.
pub fn kill_group(pid: u32, ctl: Option<&OwnedFd>) {
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

/// Starts a feeder that writes `data` to `to` and returns its pid. A
/// separate prog does the writing because a pipe holds only so much: the
/// reader may not read until it has written output the session must be
/// free to collect. The feeder reads all of `data` before it writes any of
/// it, so handing it over cannot wait on the reader.
pub fn start_feeder(data: &[u8], to: RawFd) -> Result<u32, String> {
    let [data_r, data_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
    let argv = ["mcpd", "--feed"].map(String::from);
    let pid = purgatorio::spawn(
        SELF,
        &argv,
        pctl::NEWFD,
        &[(data_r.as_raw_fd(), 0), (to, 1)],
    )
    .map_err(|e| err("stdin feeder", e))?;
    drop(data_r);
    File::from(data_w)
        .write_all(data)
        .map_err(|e| err("stdin", Error::from(e)))?;
    Ok(pid)
}

/// What a pipe surely holds: up to this much is written straight into
/// it, with no feeder.
const PIPE_ROOM: usize = 16 * 1024;

/// A pipe whose read end gives `text` and then end of file.
pub fn feed(text: &str) -> Result<OwnedFd, String> {
    let [in_r, in_w] = purgatorio::pipe().map_err(|e| err("pipe", e))?;
    if text.len() <= PIPE_ROOM {
        File::from(in_w)
            .write_all(text.as_bytes())
            .map_err(|e| err("pipe", Error::from(e)))?;
    } else {
        start_feeder(text.as_bytes(), in_w.as_raw_fd())?;
    }
    Ok(in_r)
}

/// `mcpd --exec cwd cmd argv...`: changes to `cwd` (/tmp when it is
/// empty), starts `cmd` with `argv` on this prog's three standard files,
/// waits for it and exits with its status. A failure to change directory
/// or to start the program is written to standard error and given as the
/// status after [`EXEC_FAILED`]. The program shares this prog's process
/// group, namespace and environment.
pub fn exec(args: &[String]) -> ! {
    let fail = |msg: String| -> ! {
        let _ = writeln!(std::io::stderr(), "{msg}");
        purgatorio::exits(&format!("{EXEC_FAILED}{msg}"))
    };
    let [cwd, cmd, argv @ ..] = args else {
        fail(String::from("usage: mcpd --exec cwd cmd argv..."))
    };
    if cwd.is_empty() {
        crate::session::home();
    } else if let Err(e) = purgatorio::chdir(cwd) {
        fail(err(cwd, e));
    }
    let name = argv.first().map_or(cmd.as_str(), String::as_str);
    let pid = match purgatorio::spawn(cmd, argv, pctl::NEWFD, &[(0, 0), (1, 1), (2, 2)]) {
        Ok(pid) => pid,
        Err(e) => fail(err(name, e)),
    };
    // The program has its own copies; the pipes close when it is done.
    for fd in 0..3 {
        // SAFETY: fds 0 to 2 are this prog's and nothing else here uses
        // them again.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    loop {
        match purgatorio::wait() {
            Ok(m) if m.pid == pid => purgatorio::exits(&m.status),
            Ok(_) => {}
            Err(e) => purgatorio::exits(&format!("killed: {e}")),
        }
    }
}

/// `mcpd --feed`: copy fd 0 to fd 1, reading all of it first.
pub fn feeder() {
    // SAFETY: fds 0 and 1 are the pipes `start_feeder` gave this prog.
    let (mut input, mut output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let mut data = Vec::new();
    if input.read_to_end(&mut data).is_ok() {
        drop(input);
        let _ = output.write_all(&data);
    }
}

/// `mcpd --relay`: copy fd 0 to fd 1 as it comes. The front end reads the
/// client through this prog because a pipe can be waited on and the
/// console cannot.
pub fn relay() {
    // SAFETY: fds 0 and 1 are this prog's, given by the front end.
    let (mut input, mut output) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
    let mut buf = vec![0u8; 65536];
    while let Ok(n) = input.read(&mut buf) {
        if n == 0 || output.write_all(&buf[..n]).is_err() {
            break;
        }
    }
}
