//! logfile [-size] file: make `file` with file2chan and serve it in the
//! background as a circular log. Writes append and never block; once `size`
//! bytes (default 32768) are stored, the oldest are dropped. Each reader
//! starts with the stored data and then blocks until more is written.
//! `logfile --serve size file` is the server; it writes one ready byte to
//! fd 1 before accepting I/O.

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, Request};
use purgatorio::{Context, Error};

const USAGE: &str = "usage: logfile [-size] file";

/// Inferno's logfile raises `fail: usage`, with a space.
fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("fail: usage");
}

/// Inferno's default: four times `Sys->ATOMICIO`.
const DEFAULT_SIZE: usize = 4 * 8192;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let (Some(size), Some(path)) = (args.get(2), args.get(3)) else {
            return Err(Error::from(USAGE));
        };
        let size = size.parse().map_err(|_| Error::from(USAGE))?;
        return serve(size, path);
    }
    let mut rest = &args[1..];
    let mut size = DEFAULT_SIZE;
    if let Some(flag) = rest.first().and_then(|a| a.strip_prefix('-')) {
        if flag == "-" {
            rest = &rest[1..];
        } else {
            size = flag.parse().unwrap_or(0);
            if size == 0 {
                eprintln!("logfile: can't have a zero buffer size");
                usage();
            }
            rest = &rest[1..];
        }
    }
    let [path] = rest else {
        usage();
    };
    let [r, w] = purgatorio::pipe()?;
    purgatorio::spawn(
        "/bin/logfile.wasm",
        &[
            String::from("logfile"),
            String::from("--serve"),
            size.to_string(),
            path.clone(),
        ],
        pctl::NEWFD,
        &[(w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(w);
    let mut f = std::fs::File::from(r);
    // The server exits without a ready byte if file2chan failed; it has
    // already said why.
    match file2chan::read_byte(&mut f) {
        Ok(_) => Ok(()),
        Err(_) => purgatorio::exits("fail:error"),
    }
}

/// The stored bytes and where each reader is.
struct Log {
    size: usize,
    data: VecDeque<u8>,
    /// Bytes ever written; the stored data ends here.
    total: u64,
    /// Each open file's next byte, as a count from the first write.
    readers: BTreeMap<u32, u64>,
    /// Reads waiting for data.
    pending: Vec<Request>,
}

impl Log {
    fn oldest(&self) -> u64 {
        self.total - self.data.len() as u64
    }

    /// Answers `req` if there is data for it; otherwise gives it back.
    fn try_read(&mut self, req: Request) -> Result<Option<Request>, Error> {
        let oldest = self.oldest();
        let pos = self.readers.entry(req.fid).or_insert(oldest);
        // A slow reader loses what has been overwritten.
        *pos = (*pos).max(oldest);
        if *pos >= self.total {
            return Ok(Some(req));
        }
        let start = (*pos - oldest) as usize;
        let n = (req.count() as usize).min(self.data.len() - start);
        let chunk: Vec<u8> = self.data.range(start..start + n).copied().collect();
        *pos += n as u64;
        // A reader that has gone away has closed its reply channel.
        let _ = req.respond_read(&chunk);
        Ok(None)
    }

    fn append(&mut self, bytes: &[u8]) {
        let bytes = &bytes[..bytes.len().min(self.size)];
        let over = (self.data.len() + bytes.len()).saturating_sub(self.size);
        self.data.drain(..over);
        self.data.extend(bytes);
        self.total += bytes.len() as u64;
    }

    fn wake(&mut self) -> Result<(), Error> {
        for req in std::mem::take(&mut self.pending) {
            if let Some(req) = self.try_read(req)? {
                self.pending.push(req);
            }
        }
        Ok(())
    }
}

fn serve(size: usize, path: &str) -> Result<(), Error> {
    let (dir, name) = split(path)?;
    let io = file2chan::file2chan(&dir, &name)
        .map_err(|e| Error(format!("can't create {path}: {e}")))?;
    file2chan::write_fd(1, b"R")?;
    let mut log = Log {
        size: size.max(1),
        data: VecDeque::new(),
        total: 0,
        readers: BTreeMap::new(),
        pending: Vec::new(),
    };
    loop {
        let Some(req) = io.recv()? else { return Ok(()) };
        // The kernel reuses a reply slot only after the client that owned it
        // gave up, so a waiting read on the same slot is dead.
        log.pending.retain(|p| p.reply != req.reply);
        if req.is_read() {
            if let Some(req) = log.try_read(req)? {
                log.pending.push(req);
            }
        } else if req.is_trunc() {
            // The log is append-only; opening it with OTRUNC is not an error.
            let _ = req.respond_write(0);
        } else {
            let n = req.data().len();
            log.append(req.data());
            if n > log.size {
                let _ = req.respond_err("write too long for buffer");
            } else {
                let _ = req.respond_write(n as u32);
            }
            log.wake()?;
        }
    }
}

fn split(path: &str) -> Result<(String, String), Error> {
    let path = path.strip_prefix("./").unwrap_or(path);
    match path.rsplit_once('/') {
        Some((dir, name)) if !name.is_empty() => Ok((
            if dir.is_empty() {
                String::from("/")
            } else {
                String::from(dir)
            },
            String::from(name),
        )),
        None if !path.is_empty() => Ok((String::from("."), String::from(path))),
        _ => Err(Error::from(USAGE)),
    }
}
