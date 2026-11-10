//! bytes start end [bytes]: with two arguments, print bytes `start` up to
//! `end` of standard input, with `\` doubled and any byte that is not
//! part of a valid UTF-8 character shown as `\XX`. With a third argument,
//! copy standard input to standard output with that range replaced by
//! `bytes`, in which `\n \r \t \v \\` and `\XX` (hex) are escapes. Either
//! bound may be `end`, meaning the end of the input.

use std::io::{self, Read, Write};

use purgatorio::{Context, Error};

const END: usize = usize::MAX;

fn usage() -> Error {
    Error::from("usage: bytes start end [bytes]")
}

fn bound(s: &str) -> Result<usize, Error> {
    if s == "end" {
        return Ok(END);
    }
    s.parse().map_err(|_| usage())
}

fn parse_bytes(s: &str) -> Result<Vec<u8>, Error> {
    let bad = || Error::from("invalid backslash sequence");
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let c = *b.get(i + 1).ok_or_else(bad)?;
        let esc = match c {
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'v' => Some(0x0b),
            b'\\' => Some(b'\\'),
            _ => None,
        };
        if let Some(e) = esc {
            out.push(e);
            i += 2;
            continue;
        }
        let hex = s.get(i + 1..i + 3).ok_or_else(bad)?;
        out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
        i += 3;
    }
    Ok(out)
}

fn show(data: &[u8], out: &mut dyn Write) -> io::Result<()> {
    let mut i = 0;
    while i < data.len() {
        match std::str::from_utf8(&data[i..]) {
            Ok(s) => {
                out.write_all(s.replace('\\', "\\\\").as_bytes())?;
                return Ok(());
            }
            Err(e) => {
                let ok = i + e.valid_up_to();
                let s = std::str::from_utf8(&data[i..ok]).unwrap();
                out.write_all(s.replace('\\', "\\\\").as_bytes())?;
                let bad = e.error_len().unwrap_or(data.len() - ok);
                for b in &data[ok..ok + bad] {
                    write!(out, "\\{b:02X}")?;
                }
                i = ok + bad;
            }
        }
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 3 || args.len() > 4 {
        return Err(usage());
    }
    let start = bound(&args[1])?;
    let end = bound(&args[2])?;
    if end < start {
        return Err(Error::from("out of order range"));
    }
    let new = args.get(3).map(|s| parse_bytes(s)).transpose()?;
    let mut data = Vec::new();
    io::stdin().lock().read_to_end(&mut data)?;
    let lo = start.min(data.len());
    let hi = end.min(data.len());
    let mut out = io::BufWriter::new(io::stdout().lock());
    match new {
        None => show(&data[lo..hi], &mut out)?,
        Some(b) => {
            out.write_all(&data[..lo])?;
            out.write_all(&b)?;
            out.write_all(&data[hi..])?;
        }
    }
    out.flush()?;
    Ok(())
}
