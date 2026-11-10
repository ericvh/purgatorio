//! uuencode [sourcefile] remotefile: encode a file (or standard input) as
//! printable text: a `begin mode remotefile` line, lines of up to 45
//! bytes each, an empty line and `end`.

use std::io::{self, Read, Write};

use purgatorio::{Context, DMDIR, Error};

const LEN: usize = 45;

fn code(c: u8) -> u8 {
    (c & 0x3f) + b' '
}

/// Reads until `buf` is full or the input ends.
fn fill(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

fn encode(r: &mut dyn Read, out: &mut dyn Write) -> io::Result<()> {
    let mut ib = [0u8; LEN];
    loop {
        let n = fill(r, &mut ib)?;
        if n == 0 {
            break;
        }
        let mut line = vec![code(n as u8)];
        for g in ib[..n].chunks(3) {
            let c = g[0];
            let d = g.get(1).copied().unwrap_or(0);
            let e = g.get(2).copied().unwrap_or(0);
            line.push(code(c >> 2));
            line.push(code((c << 4) & 0x30 | (d >> 4) & 0x0f));
            line.push(code((d << 2) & 0x3c | (e >> 6) & 0x03));
            line.push(code(e & 0x3f));
        }
        line.push(b'\n');
        out.write_all(&line)?;
    }
    out.write_all(&[code(0), b'\n'])
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut out = io::BufWriter::new(io::stdout().lock());
    match &args[1.min(args.len())..] {
        [remote] => {
            writeln!(out, "begin 666 {remote}")?;
            encode(&mut io::stdin().lock(), &mut out)?;
        }
        [src, remote] => {
            let Ok(mut f) = std::fs::File::open(src) else {
                return Err(Error(format!("cannot open {src}")));
            };
            let d = purgatorio::stat(src).map_err(|e| Error(format!("cannot stat {src}: {e}")))?;
            if d.mode & DMDIR != 0 {
                return Err(Error::from("cannot uuencode a directory"));
            }
            writeln!(out, "begin {:o} {remote}", d.mode & 0o777)?;
            encode(&mut f, &mut out)?;
        }
        _ => return Err(Error::from("usage: uuencode [sourcefile] remotefile")),
    }
    writeln!(out, "end")?;
    out.flush()?;
    Ok(())
}
