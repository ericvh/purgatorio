//! ramfile path: create `path` with file2chan and serve it in the background.
//! `ramfile --serve path` is the server; it writes one ready byte to fd 1
//! before accepting I/O. As in Inferno's ramfile, a bad command line or a
//! failure is reported with no status.

use std::os::fd::AsRawFd;

use purgatorio::abi::pctl;
use purgatorio::file2chan::{self, FileIO};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let path = args
            .get(2)
            .ok_or_else(|| Error::from("usage: ramfile path"))?;
        return serve(path);
    }
    let Some(path) = args.get(1) else {
        eprintln!("usage: ramfile path");
        return Ok(());
    };
    let [r, w] = purgatorio::pipe()?;
    purgatorio::spawn(
        "/bin/ramfile.wasm",
        &[
            String::from("ramfile"),
            String::from("--serve"),
            path.clone(),
        ],
        pctl::NEWFD,
        &[(w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(w);
    let mut f = std::fs::File::from(r);
    let mut b = [0u8; 1];
    // A server that fails has printed why on fd 2 before the pipe closes.
    let _ = std::io::Read::read_exact(&mut f, &mut b);
    Ok(())
}

fn serve(path: &str) -> Result<(), Error> {
    let (dir, name) = split(path)?;
    let io = file2chan::file2chan(&dir, &name)?;
    file2chan::write_fd(1, b"R")?;
    loop {
        let Some(req) = io.recv()? else { return Ok(()) };
        handle(&io, req)?;
    }
}

fn handle(_io: &FileIO, req: file2chan::Request) -> Result<(), Error> {
    let mut data = DATA.lock().unwrap_or_else(|e| e.into_inner());
    if req.is_trunc() {
        data.clear();
        return req.respond_write(0);
    }
    if req.is_read() {
        let off = req.offset as usize;
        let n = req.count() as usize;
        let slice: Vec<u8> = if off >= data.len() {
            Vec::new()
        } else {
            let end = (off + n).min(data.len());
            data[off..end].to_vec()
        };
        drop(data);
        return req.respond_read(&slice);
    }
    let off = req.offset as usize;
    let bytes = req.data().to_vec();
    if off > data.len() {
        data.resize(off, 0);
    }
    let end = off + bytes.len();
    if end > data.len() {
        data.resize(end, 0);
    }
    data[off..end].copy_from_slice(&bytes);
    drop(data);
    req.respond_write(bytes.len() as u32)
}

static DATA: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

fn split(path: &str) -> Result<(String, String), Error> {
    let path = if let Some(rest) = path.strip_prefix("./") {
        rest
    } else {
        path
    };
    match path.rsplit_once('/') {
        Some((dir, name)) if !name.is_empty() => Ok((
            if dir.is_empty() {
                String::from("/")
            } else {
                String::from(dir)
            },
            String::from(name),
        )),
        _ => Err(Error::from("usage: ramfile path")),
    }
}
