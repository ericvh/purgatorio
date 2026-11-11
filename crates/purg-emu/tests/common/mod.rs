//! Shared by the emu's test binaries: running an emu so that one that
//! dies or hangs explains itself. A failure names the exit status (with
//! the signal, for an emu that was killed by one) and shows the end of
//! the emu's standard error, where its panic and abort reports go.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How much of an emu's standard error a failure shows.
const TAIL_LINES: usize = 80;

/// Longer than any one emu run of the tests takes, on the interpreter
/// and on a loaded host; a run that takes longer is hung.
pub const LIMIT: Duration = Duration::from_secs(300);

/// The last lines of `bytes`.
pub fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(TAIL_LINES);
    let mut out = String::new();
    if skip > 0 {
        out.push_str(&format!("... ({skip} lines before)\n"));
    }
    for l in &lines[skip..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// How an emu ended, for a failure message: its status and the end of
/// its standard error.
pub fn failure(status: ExitStatus, stderr: &[u8]) -> String {
    format!("emu: {status}\n--- emu stderr\n{}---", tail(stderr))
}

/// What a child wrote to one of its pipes so far, read on a thread so
/// that the child never blocks on a full pipe.
#[derive(Clone)]
pub struct Collected(Arc<Mutex<(Vec<u8>, bool)>>);

impl Collected {
    pub fn new(mut from: impl Read + Send + 'static) -> Collected {
        let c = Collected(Arc::new(Mutex::new((Vec::new(), false))));
        let shared = c.0.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = from.read(&mut buf).unwrap_or(0);
                let mut s = shared.lock().unwrap();
                if n == 0 {
                    s.1 = true;
                    break;
                }
                s.0.extend_from_slice(&buf[..n]);
            }
        });
        c
    }

    /// The bytes so far, after waiting up to `wait` for the end of the
    /// stream.
    pub fn bytes(&self, wait: Duration) -> Vec<u8> {
        let start = Instant::now();
        loop {
            let s = self.0.lock().unwrap();
            if s.1 || start.elapsed() >= wait {
                return s.0.clone();
            }
            drop(s);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Takes the child's standard error, which must be piped, to collect it.
pub fn stderr_of(child: &mut Child) -> Collected {
    Collected::new(child.stderr.take().expect("stderr is piped"))
}

/// Waits up to `limit` for `child` to exit.
pub fn wait(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    let mut nap = Duration::from_micros(200);
    loop {
        if let Some(status) = child.try_wait().expect("wait for emu") {
            return Some(status);
        }
        if start.elapsed() >= limit {
            return None;
        }
        std::thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(10));
    }
}

/// What became of an emu that stopped answering or was expected to have
/// exited: its status if it died, or that it was still running (it is
/// killed), and the end of its standard error.
pub fn post_mortem(child: &mut Child, stderr: &Collected) -> String {
    let state = match wait(child, Duration::from_secs(2)) {
        Some(status) => format!("emu: {status}"),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            String::from("emu: still running (killed by the test)")
        }
    };
    format!(
        "{state}\n--- emu stderr\n{}---",
        tail(&stderr.bytes(Duration::from_secs(2)))
    )
}

/// An emu a test talks to while it runs. Its standard error is collected;
/// when the test fails (the thread is panicking as this is dropped), what
/// became of the emu is printed with the failure. The emu is killed on
/// drop.
pub struct Watched {
    pub child: Child,
    stderr: Collected,
}

impl Watched {
    /// Spawns `cmd` with its standard error piped; the caller sets up
    /// its standard input and output.
    pub fn spawn(cmd: &mut Command) -> Watched {
        let mut child = cmd.stderr(Stdio::piped()).spawn().expect("spawn emu");
        let stderr = stderr_of(&mut child);
        Watched { child, stderr }
    }

    /// See [`post_mortem`].
    pub fn post_mortem(&mut self) -> String {
        post_mortem(&mut self.child, &self.stderr)
    }

    /// Waits up to `limit` for the emu to exit by itself and returns its
    /// status and standard error; panics, with what became of it, if it
    /// does not.
    pub fn finish(&mut self, limit: Duration) -> (ExitStatus, Vec<u8>) {
        match wait(&mut self.child, limit) {
            Some(status) => (status, self.stderr.bytes(Duration::from_secs(2))),
            None => panic!("the emu did not exit\n{}", self.post_mortem()),
        }
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", self.post_mortem());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `cmd` to its end with `stdin` as its standard input, like
/// `Command::output`, but for at most [`LIMIT`]: an emu that runs longer
/// is killed, and the test fails with what it wrote.
pub fn output(cmd: &mut Command, stdin: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn emu");
    let out = Collected::new(child.stdout.take().unwrap());
    let err = stderr_of(&mut child);
    // An emu that exits without reading its input closes the pipe.
    let _ = child.stdin.take().unwrap().write_all(stdin);
    match wait(&mut child, LIMIT) {
        Some(status) => Output {
            status,
            stdout: out.bytes(LIMIT),
            stderr: err.bytes(LIMIT),
        },
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let grace = Duration::from_secs(2);
            panic!(
                "emu ran for more than {LIMIT:?} and was killed\n--- emu stdout\n{}--- emu stderr\n{}---",
                tail(&out.bytes(grace)),
                tail(&err.bytes(grace))
            );
        }
    }
}
