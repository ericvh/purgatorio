//! uudecode [-p] [encodedfile ...]: decode uuencoded data, skipping any
//! lines before `begin mode name`, and write it to `name` with that mode,
//! or to standard output with `-p`.

use std::io::{self, BufRead, BufReader, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Dir, Error};

const LEN: usize = 45;

fn code(c: u8) -> usize {
    usize::from(c.wrapping_sub(b' ') & 0x3f)
}

fn line(r: &mut dyn BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut b = Vec::new();
    if r.read_until(b'\n', &mut b)? == 0 {
        return Ok(None);
    }
    Ok(Some(b))
}

fn decode(r: &mut dyn BufRead, to_stdout: bool) -> Result<(), Error> {
    let (mode, name) = loop {
        let Some(l) = line(r)? else {
            return Err(Error::from("no begin line"));
        };
        if l.starts_with(b"begin ") {
            let s = String::from_utf8_lossy(&l).into_owned();
            let f: Vec<&str> = s.split([' ', '\n']).filter(|w| !w.is_empty()).collect();
            if f.len() < 3 {
                return Err(Error::from("bad begin line"));
            }
            break (
                u32::from_str_radix(f[1], 8).unwrap_or(0o666),
                String::from(f[2]),
            );
        }
    };
    let mut out: Box<dyn Write> = if to_stdout {
        Box::new(io::stdout().lock())
    } else {
        let fd = purgatorio::create(&name, purgatorio::abi::mode::OWRITE, 0o666)
            .map_err(|e| Error(format!("cannot create {name}: {e}")))?;
        Box::new(std::fs::File::from(fd))
    };
    loop {
        let Some(b) = line(r)? else {
            return Err(Error::from("missing end line"));
        };
        let n = code(b[0]);
        if n == 0 {
            if b.len() != 2 || b[1] != b'\n' {
                return Err(Error::from("bad 0 count line"));
            }
            break;
        }
        if n > LEN {
            return Err(Error::from("too many bytes on line"));
        }
        let need = n.div_ceil(3) * 4;
        if b.len() < 1 + need {
            return Err(Error::from("bad uuencode count"));
        }
        let mut ob = Vec::with_capacity(n);
        for g in b[1..1 + need].chunks(4) {
            let (a, bb, c, d) = (code(g[0]), code(g[1]), code(g[2]), code(g[3]));
            ob.push((a << 2 | bb >> 4) as u8);
            ob.push((bb << 4 | c >> 2) as u8);
            ob.push((c << 6 | d) as u8);
        }
        ob.truncate(n);
        out.write_all(&ob)?;
    }
    out.flush()?;
    match line(r)? {
        Some(l) if l.starts_with(b"end") => {}
        _ => return Err(Error::from("missing end line")),
    }
    if !to_stdout {
        let mut d = Dir::null();
        d.mode = mode & 0o777;
        purgatorio::wstat(&name, &d).map_err(|e| Error(format!("cannot wstat {name}: {e}")))?;
    }
    Ok(())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // Inferno's uudecode prints its usage, or any error, alone and exits
    // with no status.
    run(args).map_err(|e| {
        eprintln!("{e}");
        purgatorio::exits("")
    })
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "uudecode [-p] [encodedfile...]");
    let mut to_stdout = false;
    while let Some(o) = arg.opt() {
        match o {
            'p' => to_stdout = true,
            _ => return Err(arg.usage()),
        }
    }
    let files = arg.argv();
    if files.is_empty() {
        return decode(&mut io::stdin().lock(), to_stdout);
    }
    for f in &files {
        let Ok(file) = std::fs::File::open(f) else {
            return Err(Error(format!("cannot open {f}")));
        };
        decode(&mut BufReader::new(file), to_stdout)?;
    }
    Ok(())
}
