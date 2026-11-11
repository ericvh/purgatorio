//! `file2chan`: a file whose reads and writes are delivered to this prog.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use purg_abi::fio;

use crate::{Error, check, sys};

/// The read and write request channels for one served file.
pub struct FileIO {
    read: i32,
    write: i32,
}

impl Drop for FileIO {
    fn drop(&mut self) {
        unsafe {
            sys::chan_close(self.read);
            sys::chan_close(self.write);
        }
    }
}

/// Creates `dir/name` and returns the channels that carry its I/O. `dir`
/// does not have to be on `#s`; the file is bound over `dir/name`.
pub fn file2chan(dir: &str, name: &str) -> Result<FileIO, Error> {
    let mut handles = [-1i32; 2];
    check(unsafe {
        sys::file2chan(
            dir.as_ptr(),
            dir.len(),
            name.as_ptr(),
            name.len(),
            handles.as_mut_ptr(),
        )
    })?;
    Ok(FileIO {
        read: handles[0],
        write: handles[1],
    })
}

/// One request from a client of the file.
pub struct Request {
    pub offset: u64,
    pub fid: u32,
    pub reply: i32,
    body: Body,
}

enum Body {
    Read {
        count: u32,
    },
    Write {
        data: Vec<u8>,
    },
    Trunc,
    /// The fid was closed: a write with no data and no reply channel.
    Close,
}

impl Request {
    pub fn is_read(&self) -> bool {
        matches!(self.body, Body::Read { .. })
    }

    pub fn is_trunc(&self) -> bool {
        matches!(self.body, Body::Trunc)
    }

    /// Whether this is the close of `fid`, which takes no reply. Only
    /// [`FileIO::recv_with_close`] and [`recv_any_with_close`] return
    /// these.
    pub fn is_close(&self) -> bool {
        matches!(self.body, Body::Close)
    }

    pub fn count(&self) -> u32 {
        match self.body {
            Body::Read { count } => count,
            Body::Write { ref data } => data.len() as u32,
            Body::Trunc | Body::Close => 0,
        }
    }

    pub fn data(&self) -> &[u8] {
        match &self.body {
            Body::Write { data } => data,
            _ => &[],
        }
    }

    /// Replies to a read with `data`.
    pub fn respond_read(&self, data: &[u8]) -> Result<(), Error> {
        let mut msg = vec![fio::OK_READ];
        msg.extend_from_slice(data);
        reply(self.reply, &msg)
    }

    /// Replies to a write with the count that was stored.
    pub fn respond_write(&self, n: u32) -> Result<(), Error> {
        let mut msg = vec![fio::OK_WRITE];
        msg.extend_from_slice(&n.to_le_bytes());
        reply(self.reply, &msg)
    }

    /// Fails the request with an Inferno error string.
    pub fn respond_err(&self, msg: &str) -> Result<(), Error> {
        let mut buf = vec![fio::ERR];
        buf.extend_from_slice(msg.as_bytes());
        reply(self.reply, &buf)
    }
}

fn reply(h: i32, msg: &[u8]) -> Result<(), Error> {
    // A close has no reply channel.
    if h < 0 {
        return Ok(());
    }
    // The kernel owns the reply handle and closes it after this send. Closing
    // it here races with the next request reusing the same slot.
    check(unsafe { sys::chan_send(h, msg.as_ptr(), msg.len(), std::ptr::null(), 0) })?;
    Ok(())
}

impl FileIO {
    /// Blocks until the next read, write, or truncate. `Ok(None)` means the
    /// file's channels have hung up. Closes are skipped.
    pub fn recv(&self) -> Result<Option<Request>, Error> {
        Ok(recv_any(&[self])?.map(|(_, r)| r))
    }

    /// [`FileIO::recv`], also returning each close of a fid
    /// ([`Request::is_close`]).
    pub fn recv_with_close(&self) -> Result<Option<Request>, Error> {
        Ok(recv_any_with_close(&[self])?.map(|(_, r)| r))
    }
}

/// Blocks until the next request on any of `files`, and returns it with the
/// index of its file. `Ok(None)` means that file's channels have hung up.
/// Closes are skipped.
pub fn recv_any(files: &[&FileIO]) -> Result<Option<(usize, Request)>, Error> {
    loop {
        match recv_any_with_close(files)? {
            Some((_, r)) if r.is_close() => continue,
            other => return Ok(other),
        }
    }
}

/// [`recv_any`], also returning each close of a fid.
pub fn recv_any_with_close(files: &[&FileIO]) -> Result<Option<(usize, Request)>, Error> {
    let ops: Vec<i32> = files.iter().flat_map(|f| [2, f.read, 2, f.write]).collect();
    let which = unsafe { sys::wait(ops.as_ptr(), ops.len() / 2, -1) };
    if which < 0 {
        return Err(Error::last());
    }
    let which = which as usize;
    let f = files[which / 2];
    let h = if which.is_multiple_of(2) {
        f.read
    } else {
        f.write
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut handles = [-1i32; 4];
    let n = unsafe { sys::chan_recv(h, buf.as_mut_ptr(), buf.len(), handles.as_mut_ptr(), 0) };
    if n < 0 {
        let err = Error::last();
        if err.0 == purg_abi::error::EHUNGUP {
            return Ok(None);
        }
        return Err(err);
    }
    buf.truncate(n as usize);
    decode(&buf).map(|r| Some((which / 2, r)))
}

fn decode(buf: &[u8]) -> Result<Request, Error> {
    if buf.is_empty() {
        return Err(Error::from(purg_abi::error::EIO));
    }
    match buf[0] {
        fio::READ => {
            let (offset, count, fid, reply) = fio::unpack_read(buf).ok_or_else(Error::last)?;
            Ok(Request {
                offset,
                fid,
                reply,
                body: Body::Read { count },
            })
        }
        fio::WRITE => {
            let (offset, fid, reply, data) = fio::unpack_write(buf).ok_or_else(Error::last)?;
            let body = if reply < 0 && data.is_empty() {
                Body::Close
            } else {
                Body::Write {
                    data: data.to_vec(),
                }
            };
            Ok(Request {
                offset,
                fid,
                reply,
                body,
            })
        }
        fio::TRUNC => {
            if buf.len() < fio::WRITE_HDR {
                return Err(Error::from(purg_abi::error::EIO));
            }
            let fid = u32::from_le_bytes(buf[9..13].try_into().unwrap_or([0; 4]));
            let reply = i32::from_le_bytes(buf[13..17].try_into().unwrap_or([0; 4]));
            Ok(Request {
                offset: 0,
                fid,
                reply,
                body: Body::Trunc,
            })
        }
        _ => Err(Error::from(purg_abi::error::EIO)),
    }
}

/// Copies `src` to `dst` without closing `dst`. Used to signal a parent
/// without dropping its pipe end.
pub fn write_fd(fd: i32, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    f.write_all(bytes)?;
    f.flush()
}

/// Reads one byte, for a parent waiting until a server is ready.
pub fn read_byte(mut r: impl Read) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}
