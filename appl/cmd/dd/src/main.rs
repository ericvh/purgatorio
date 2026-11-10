//! dd [-option value] ...: copy a file in blocks, with conversions.
//!
//! Options: `-if f`, `-of f`, `-ibs n`, `-obs n`, `-bs n` (both, and with
//! no conversion each input block is written as it was read), `-cbs n`,
//! `-skip n` (read and drop n input blocks), `-iseek n`, `-oseek n` (seek
//! n blocks on input or output), `-count n`, `-files n`, and
//! `-conv c,...` with `block`, `unblock`, `lcase`, `ucase`, `swab`,
//! `noerror` and `sync`. A size may end in `k` (1024) or `b` (512), and
//! `x` multiplies two sizes. The record counts go to standard error.
//!
//! The EBCDIC conversions (`ascii`, `ebcdic`, `ibm`) are not provided.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;

use purgatorio::{Context, Error};

#[derive(Default)]
struct Conv {
    block: bool,
    unblock: bool,
    lcase: bool,
    ucase: bool,
    swab: bool,
    noerror: bool,
    sync: bool,
}

impl Conv {
    fn any(&self) -> bool {
        self.block || self.unblock || self.lcase || self.ucase || self.swab || self.sync
    }
}

fn number(key: &str, s: &str) -> Result<u64, Error> {
    let mut total = 1u64;
    for part in s.split('x') {
        let (digits, mul) = match part.as_bytes().last() {
            Some(b'k') => (&part[..part.len() - 1], 1024),
            Some(b'b') => (&part[..part.len() - 1], 512),
            _ => (part, 1),
        };
        let n: u64 = digits
            .parse()
            .map_err(|_| Error(format!("bad number for {key}: {s}")))?;
        total = total
            .checked_mul(n)
            .and_then(|t| t.checked_mul(mul))
            .ok_or_else(|| Error::from("argument out of range"))?;
    }
    Ok(total)
}

#[derive(Default)]
struct Stats {
    nifr: u64,
    nipr: u64,
    nofr: u64,
    nopr: u64,
    ntrunc: u64,
}

struct Out {
    w: Box<dyn Write>,
    obs: usize,
    buf: Vec<u8>,
}

impl Out {
    fn write_block(&mut self, b: &[u8], st: &mut Stats) -> io::Result<()> {
        self.w.write_all(b)?;
        if b.len() == self.obs {
            st.nofr += 1;
        } else {
            st.nopr += 1;
        }
        Ok(())
    }

    fn push(&mut self, data: &[u8], st: &mut Stats) -> io::Result<()> {
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while self.buf.len() - start >= self.obs {
            let b = self.buf[start..start + self.obs].to_vec();
            self.write_block(&b, st)?;
            start += self.obs;
        }
        self.buf.drain(..start);
        Ok(())
    }

    fn flush(&mut self, st: &mut Stats) -> io::Result<()> {
        if !self.buf.is_empty() {
            let b = std::mem::take(&mut self.buf);
            self.write_block(&b, st)?;
        }
        self.w.flush()
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut ifile = None;
    let mut ofile = None;
    let (mut ibs, mut obs, mut bs, mut cbs) = (512u64, 512u64, 0u64, 0u64);
    let (mut skip, mut iseek, mut oseek, mut count, mut files) = (0u64, 0u64, 0u64, None, 1u64);
    let mut conv = Conv::default();
    let mut i = 1;
    while i < args.len() {
        let Some(key) = args[i].strip_prefix('-') else {
            return Err(Error(format!("bad arg: {}", args[i])));
        };
        let Some(val) = args.get(i + 1) else {
            return Err(Error(format!("arg {key} needs a value")));
        };
        i += 2;
        match key {
            "if" => ifile = Some(val.clone()),
            "of" => ofile = Some(val.clone()),
            "ibs" => ibs = number(key, val)?,
            "obs" => obs = number(key, val)?,
            "bs" => bs = number(key, val)?,
            "cbs" => cbs = number(key, val)?,
            "skip" => skip = number(key, val)?,
            "iseek" | "seek" => iseek = number(key, val)?,
            "oseek" => oseek = number(key, val)?,
            "count" => count = Some(number(key, val)?),
            "files" => files = number(key, val)?,
            "conv" => {
                for c in val.split(',') {
                    match c {
                        "block" => conv.block = true,
                        "unblock" => conv.unblock = true,
                        "lcase" => conv.lcase = true,
                        "ucase" => conv.ucase = true,
                        "swab" => conv.swab = true,
                        "noerror" => conv.noerror = true,
                        "sync" => conv.sync = true,
                        _ => return Err(Error(format!("bad arg: {c}"))),
                    }
                }
            }
            _ => return Err(Error(format!("bad arg: {key}"))),
        }
    }
    if bs != 0 {
        ibs = bs;
        obs = bs;
    }
    if ibs == 0 || obs == 0 {
        return Err(Error::from("counts: cannot be zero"));
    }
    let direct = bs != 0 && !conv.any();
    let (ibs, obs) = (
        usize::try_from(ibs).map_err(|_| Error::from("argument out of range"))?,
        usize::try_from(obs).map_err(|_| Error::from("argument out of range"))?,
    );
    let cbs = usize::try_from(cbs).map_err(|_| Error::from("argument out of range"))?;
    let mut input: Box<dyn Read> = match &ifile {
        Some(f) => {
            let mut file =
                File::open(f).map_err(|_| Error(format!("open {f}: {}", purgatorio::errstr())))?;
            if iseek > 0 {
                file.seek(SeekFrom::Start(iseek * ibs as u64))?;
            }
            Box::new(file)
        }
        None => {
            // SAFETY: fd 0 outlives the copy; ManuallyDrop leaves it open.
            let mut f = ManuallyDrop::new(unsafe { File::from_raw_fd(0) });
            if iseek > 0 {
                f.seek(SeekFrom::Start(iseek * ibs as u64))?;
            }
            Box::new(Stdin(f))
        }
    };
    let w: Box<dyn Write> = match &ofile {
        Some(f) => {
            let mut file = if oseek > 0 {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(f)
            } else {
                File::create(f)
            }
            .map_err(|_| Error(format!("create {f}: {}", purgatorio::errstr())))?;
            if oseek > 0 {
                file.seek(SeekFrom::Start(oseek * obs as u64))?;
            }
            Box::new(file)
        }
        None => Box::new(io::stdout().lock()),
    };
    let mut out = Out {
        w,
        obs,
        buf: Vec::new(),
    };
    let mut st = Stats::default();
    let mut buf = vec![0u8; ibs];
    // `block` state: the line being collected; `unblock` state: the
    // partial record.
    let mut cline: Vec<u8> = Vec::new();
    let mut crec: Vec<u8> = Vec::new();
    let mut skipped = 0u64;
    let mut records = 0u64;
    let mut ended = 0u64;
    let pad = if conv.block || conv.unblock { b' ' } else { 0 };
    loop {
        if count.is_some_and(|c| records >= c) {
            break;
        }
        let n = match input.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("dd: read error: {}", Error::from(e));
                if conv.noerror {
                    continue;
                }
                break;
            }
        };
        if n == 0 {
            ended += 1;
            if ended >= files {
                break;
            }
            continue;
        }
        if skipped < skip {
            skipped += 1;
            continue;
        }
        records += 1;
        if n == ibs {
            st.nifr += 1;
        } else {
            st.nipr += 1;
        }
        let mut rec = buf[..n].to_vec();
        if conv.sync && rec.len() < ibs {
            rec.resize(ibs, pad);
        }
        if direct {
            out.write_block(&rec, &mut st)?;
            continue;
        }
        if conv.swab {
            for p in rec.as_chunks_mut::<2>().0 {
                p.swap(0, 1);
            }
        }
        if conv.lcase {
            rec.make_ascii_lowercase();
        }
        if conv.ucase {
            rec.make_ascii_uppercase();
        }
        if conv.block && cbs > 0 {
            for &b in &rec {
                if b == b'\n' {
                    let mut r = std::mem::take(&mut cline);
                    if r.len() > cbs {
                        r.truncate(cbs);
                        st.ntrunc += 1;
                    }
                    r.resize(cbs, b' ');
                    out.push(&r, &mut st)?;
                } else {
                    cline.push(b);
                }
            }
        } else if conv.unblock && cbs > 0 {
            for &b in &rec {
                crec.push(b);
                if crec.len() == cbs {
                    let r = std::mem::take(&mut crec);
                    let end = r.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
                    out.push(&r[..end], &mut st)?;
                    out.push(b"\n", &mut st)?;
                }
            }
        } else {
            out.push(&rec, &mut st)?;
        }
    }
    if conv.block && cbs > 0 && !cline.is_empty() {
        let mut r = std::mem::take(&mut cline);
        if r.len() > cbs {
            r.truncate(cbs);
            st.ntrunc += 1;
        }
        r.resize(cbs, b' ');
        out.push(&r, &mut st)?;
    }
    if conv.unblock && cbs > 0 && !crec.is_empty() {
        let end = crec.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
        out.push(&crec[..end], &mut st)?;
        out.push(b"\n", &mut st)?;
    }
    out.flush(&mut st)?;
    eprintln!("{}+{} records in", st.nifr, st.nipr);
    eprintln!("{}+{} records out", st.nofr, st.nopr);
    if st.ntrunc > 0 {
        eprintln!("{} truncated records", st.ntrunc);
    }
    Ok(())
}

/// Standard input read straight from fd 0, so that one read is one record.
struct Stdin(ManuallyDrop<File>);

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
