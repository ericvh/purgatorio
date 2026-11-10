//! read [-[eor] offset] [count]: one read of up to `count` bytes (default
//! 8192) from standard input, copied to standard output. `-o`, `-e` and
//! `-r` first seek `offset` bytes from the start, the end or the current
//! offset. At end of file the exit status is `fail:eof`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::{Context, Error};

const USAGE: &str = "usage: read [-[ero] offset] count";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut rest = &args[1.min(args.len())..];
    let mut seek = None;
    if let Some(flag) = rest.first().filter(|a| a.starts_with('-')) {
        let off = rest.get(1).ok_or_else(|| Error::from(USAGE))?;
        let off: i64 = off.parse().map_err(|_| Error::from(USAGE))?;
        seek = Some(match flag.as_str() {
            "-o" => SeekFrom::Start(u64::try_from(off).map_err(|_| Error::from(USAGE))?),
            "-e" => SeekFrom::End(off),
            "-r" => SeekFrom::Current(off),
            _ => return Err(Error::from(USAGE)),
        });
        rest = &rest[2..];
    }
    let count: usize = match rest {
        [] => 8192,
        [n] => n.parse().map_err(|_| Error::from(USAGE))?,
        _ => return Err(Error::from(USAGE)),
    };
    // SAFETY: fd 0 stays open for the life of the prog; ManuallyDrop keeps
    // the File from closing it.
    let mut stdin = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
    if let Some(s) = seek {
        // Like sys->seek, a failed seek is not an error here; the read
        // that follows reports any trouble.
        let _ = stdin.seek(s);
    }
    if count == 0 {
        return Ok(());
    }
    let mut buf = vec![0u8; count];
    match stdin.read(&mut buf) {
        Ok(0) => purgatorio::exits("fail:eof"),
        Ok(n) => {
            let mut out = std::io::stdout().lock();
            out.write_all(&buf[..n])?;
            out.flush()?;
            Ok(())
        }
        Err(e) => {
            eprintln!("read: read error: {}", Error::from(e));
            purgatorio::exits("fail:error");
        }
    }
}
