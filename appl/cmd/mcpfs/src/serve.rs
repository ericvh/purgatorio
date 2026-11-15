//! The file server's loop: 9P from the mount and JSON-RPC from the MCP
//! server, both at once, so that a tool call that takes long does not hold
//! up the rest of the mount.
//!
//! A read of a `call` file whose result is not yet known sends
//! `tools/call` and leaves the read waiting; the server's answer, matched
//! by its request id, ends it. Every other request is answered at once by
//! [`dispatch`], as before. `Tflush` of a waiting read ends it at once and,
//! when no other read waits for that call, sends the server
//! `notifications/cancelled`. With a timeout (`mcpfs -t`), a call not
//! answered in time fails with `timed out` and is cancelled the same way.
//! When the mount goes, the loop ends at once, answered calls or not.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::os::fd::AsRawFd;
use std::time::Instant;

use purg_styx::server::{MAX_MSIZE, dispatch};
use purg_styx::{Fcall, Msg};
use purgatorio::{Error, Ready};

use crate::json::{self, Value};
use crate::{MAX_LINE, McpFs};

/// A read of a `call` file waiting for the server's answer.
struct Waiter {
    tag: u16,
    offset: u64,
    count: u32,
}

/// A `tools/call` the server has not yet answered.
struct Pending {
    fid: u32,
    waiters: Vec<Waiter>,
    /// When it fails with `timed out`, in ms since the loop started.
    deadline: Option<u128>,
}

/// The error a call that ran out of time fails with.
const ETIMEDOUT: &str = "timed out";

/// Serves 9P on `io` until the mount goes. `timeout_ms` bounds each call.
pub fn serve(io: File, fs: &mut McpFs, timeout_ms: Option<u64>) -> Result<(), Error> {
    let start = Instant::now();
    let now = || start.elapsed().as_millis();
    let mut msize = MAX_MSIZE;
    let mut pending: BTreeMap<i64, Pending> = BTreeMap::new();
    let io_fd = io.as_raw_fd();
    let mcp_fd = fs.client.from.get_ref().as_raw_fd();
    loop {
        // Whole lines the reader already holds need no wait.
        while fs.client.from.buffer().contains(&b'\n') {
            mcp_line(fs, &io, &mut pending)?;
        }
        let wait = pending
            .values()
            .filter_map(|p| p.deadline)
            .min()
            .map_or(-1, |d| {
                i32::try_from(d.saturating_sub(now())).unwrap_or(i32::MAX)
            });
        match purgatorio::ready(&[Ready::Read(io_fd), Ready::Read(mcp_fd)], wait)? {
            Some(0) => {
                let buf = match read_msg(&mut &io, msize) {
                    Ok(b) => b,
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                    Err(e) => return Err(Error(e.to_string())),
                };
                let msg = Msg::decode(&buf, msize).map_err(|e| Error(e.to_string()))?;
                let deadline = timeout_ms.map(|t| now() + u128::from(t));
                request(fs, &io, &msg, &mut msize, &mut pending, deadline)?;
            }
            Some(_) => mcp_line(fs, &io, &mut pending)?,
            None => {}
        }
        let late: Vec<i64> = pending
            .iter()
            .filter(|(_, p)| p.deadline.is_some_and(|d| d <= now()))
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            fs.cancel(id, ETIMEDOUT);
            if let Some(p) = pending.remove(&id) {
                finish(fs, &io, p, Err(String::from(ETIMEDOUT)), id)?;
            }
        }
    }
}

/// One 9P request: a read that must wait for a call, a flush, or
/// anything else, which is answered now.
fn request(
    fs: &mut McpFs,
    io: &File,
    msg: &Msg<'_>,
    msize: &mut u32,
    pending: &mut BTreeMap<i64, Pending>,
    deadline: Option<u128>,
) -> Result<(), Error> {
    let tag = msg.tag;
    match msg.fcall {
        Fcall::Tread { fid, offset, count } if fs.needs_call(fid) => {
            let id = match fs.in_flight(fid) {
                Some(id) => id,
                None => match fs.start_call(fid) {
                    Ok(id) => {
                        pending.insert(
                            id,
                            Pending {
                                fid,
                                waiters: Vec::new(),
                                deadline,
                            },
                        );
                        id
                    }
                    // Bad arguments: the read fails now, as before.
                    Err(e) => return reply(io, tag, Fcall::Rerror { ename: &e }),
                },
            };
            if let Some(p) = pending.get_mut(&id) {
                p.waiters.push(Waiter { tag, offset, count });
            }
            Ok(())
        }
        Fcall::Tflush { oldtag } => {
            let found = pending
                .iter_mut()
                .find(|(_, p)| p.waiters.iter().any(|w| w.tag == oldtag));
            if let Some((&id, p)) = found {
                p.waiters.retain(|w| w.tag != oldtag);
                if p.waiters.is_empty() {
                    fs.cancel(id, "the read was flushed");
                    if let Some(p) = pending.remove(&id) {
                        fs.forget(p.fid, id);
                    }
                }
            }
            reply(io, tag, Fcall::Rflush)
        }
        _ => {
            let out = dispatch(fs, msg, msize);
            write_all(io, &out)
        }
    }
}

/// One line from the MCP server: the answer to a call, a request of its
/// own (answered), or a notification (dropped).
fn mcp_line(fs: &mut McpFs, io: &File, pending: &mut BTreeMap<i64, Pending>) -> Result<(), Error> {
    let mut line = Vec::new();
    let n = (&mut fs.client.from)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(|e| Error(format!("mcp server: {}", Error::from(e))))?;
    if n == 0 {
        // The server has gone: every waiting read fails.
        for (id, p) in core::mem::take(pending) {
            finish(fs, io, p, Err(String::from("mcp server: hungup")), id)?;
        }
        return Err(Error::from("mcp server: hungup"));
    }
    let Ok(v) = json::parse(String::from_utf8_lossy(&line).trim()) else {
        return Ok(());
    };
    if v.get("method").is_some() {
        if let Some(rid) = v.get("id") {
            fs.client.answer(&v, rid.clone()).map_err(Error)?;
        }
        return Ok(());
    }
    let Some(id) = v.get("id").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some(p) = pending.remove(&id) else {
        return Ok(());
    };
    let result = match (v.get("error"), v.get("result")) {
        (Some(e), _) => Err(String::from(
            e.get("message").and_then(Value::as_str).unwrap_or("error"),
        )),
        (None, Some(r)) => {
            let mut out = r.encode().into_bytes();
            out.push(b'\n');
            Ok(out)
        }
        (None, None) => Err(String::from("mcp server: reply without a result")),
    };
    finish(fs, io, p, result, id)
}

/// Ends a call: keeps its result on its fid for later reads, and answers
/// every read waiting for it.
fn finish(
    fs: &mut McpFs,
    io: &File,
    p: Pending,
    result: Result<Vec<u8>, String>,
    id: i64,
) -> Result<(), Error> {
    let end = fs.settle(p.fid, id, result.clone());
    for w in p.waiters {
        match &result {
            Ok(data) => {
                let at = if w.offset >= end {
                    w.offset - end
                } else {
                    w.offset
                };
                let data = crate::slice(data, at, w.count);
                reply(io, w.tag, Fcall::Rread { data: &data })?;
            }
            Err(e) => reply(io, w.tag, Fcall::Rerror { ename: e })?,
        }
    }
    Ok(())
}

fn reply(io: &File, tag: u16, fcall: Fcall<'_>) -> Result<(), Error> {
    let out = Msg { tag, fcall }
        .encode()
        .map_err(|e| Error(e.to_string()))?;
    write_all(io, &out)
}

fn write_all(mut io: &File, data: &[u8]) -> Result<(), Error> {
    std::io::Write::write_all(&mut io, data).map_err(|e| Error(e.to_string()))
}

fn read_msg(io: &mut impl Read, msize: u32) -> io::Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    io.read_exact(&mut hdr)?;
    let n = u32::from_le_bytes(hdr) as usize;
    if n < 7 || n > msize as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad 9P size"));
    }
    let mut buf = vec![0u8; n];
    buf[..4].copy_from_slice(&hdr);
    io.read_exact(&mut buf[4..])?;
    Ok(buf)
}
