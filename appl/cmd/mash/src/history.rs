//! `load history`: two files served by helper progs, as mash's history
//! module serves them.
//!
//! - `/chan/history` holds the commands typed at the console. Reading it
//!   gives them; writing adds lines to it.
//! - `/chan/mash` takes commands for the console session: text written to
//!   it is run as if typed, at the next prompt.
//!
//! The helpers are this command started as `mash --history` and
//! `mash --serve`; each writes one byte to fd 1 once its file exists.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use purgatorio::abi::{mode, pctl};
use purgatorio::file2chan::{self, Request};
use purgatorio::{Error, file2chan::FileIO};

use crate::exec::MASH;

pub const HISTORY: &str = "/chan/history";
pub const MASHF: &str = "/chan/mash";

pub struct History {
    pub pids: Vec<u32>,
    hist: Option<OwnedFd>,
}

fn helper(flag: &str) -> Result<u32, Error> {
    let [r, w] = purgatorio::pipe()?;
    let null = purgatorio::open("/dev/null", mode::OREAD)?;
    let pid = purgatorio::spawn(
        MASH,
        &[String::from("mash"), String::from(flag)],
        pctl::NEWFD,
        &[(null.as_raw_fd(), 0), (w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(w);
    file2chan::read_byte(File::from(r))?;
    Ok(pid)
}

impl History {
    pub fn start() -> Result<History, Error> {
        let pids = vec![helper("--history")?, helper("--serve")?];
        let hist = purgatorio::open(HISTORY, mode::OWRITE).ok();
        Ok(History { pids, hist })
    }

    /// Adds a typed line to the history.
    pub fn record(&self, line: &str) {
        if let Some(fd) = &self.hist {
            let _ = file2chan::write_fd(fd.as_raw_fd(), line.as_bytes());
        }
    }

    /// Text written to `/chan/mash` since the last call.
    pub fn pending(&self) -> String {
        let Ok(fd) = purgatorio::open(MASHF, mode::OREAD) else {
            return String::new();
        };
        let mut s = String::new();
        let _ = File::from(fd).read_to_string(&mut s);
        s
    }

    /// Stops the helpers.
    pub fn stop(&self) {
        for pid in &self.pids {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .write(true)
                .open(format!("/prog/{pid}/ctl"))
            {
                let _ = f.write_all(b"kill");
            }
        }
    }
}

fn reply_read(req: &Request, data: &[u8]) -> Result<(), Error> {
    let off = (req.offset as usize).min(data.len());
    let end = (off + req.count() as usize).min(data.len());
    req.respond_read(&data[off..end])
}

fn serve_file(
    name: &str,
    mut handle: impl FnMut(&FileIO, Request) -> Result<(), Error>,
) -> Result<(), Error> {
    let io = file2chan::file2chan("/chan", name)?;
    file2chan::write_fd(1, b"R")?;
    // mash kills its helpers when it exits, which ends a pending recv.
    while let Ok(Some(req)) = io.recv() {
        handle(&io, req)?;
    }
    Ok(())
}

/// `mash --history`: serves `/chan/history`.
pub fn serve_history() -> Result<(), Error> {
    let mut text: Vec<u8> = Vec::new();
    serve_file("history", |_, req| {
        if req.is_read() {
            reply_read(&req, &text)
        } else if req.is_trunc() {
            text.clear();
            req.respond_write(0)
        } else {
            text.extend_from_slice(req.data());
            req.respond_write(req.data().len() as u32)
        }
    })
}

/// `mash --serve`: serves `/chan/mash`. A read takes the text written
/// since the last read.
pub fn serve_mash() -> Result<(), Error> {
    let mut queue: Vec<u8> = Vec::new();
    serve_file("mash", |_, req| {
        if req.is_read() {
            let n = (req.count() as usize).min(queue.len());
            let data: Vec<u8> = queue.drain(..n).collect();
            req.respond_read(&data)
        } else if req.is_trunc() {
            req.respond_write(0)
        } else {
            queue.extend_from_slice(req.data());
            req.respond_write(req.data().len() as u32)
        }
    })
}
