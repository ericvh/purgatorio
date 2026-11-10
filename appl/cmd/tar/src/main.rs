//! tar key [file ...]: create, list or extract a ustar archive.
//!
//! Inferno has no tar(1), so the key follows Plan 9's: one function letter,
//! `c` (create), `t` (list) or `x` (extract), plus modifiers:
//!
//! - `f`: the next argument names the archive instead of standard
//!   input or output.
//! - `v`: print names as they are added or extracted (on standard
//!   error); with `t`, list modes, sizes and dates too.
//! - `z`: gzip the archive on `c`. `t` and `x` recognise a gzipped
//!   archive whether or not `z` is given.
//! - `k`: with `x`, keep existing files.
//! - `m`: with `x`, do not set modification times from the archive.
//! - `T`: with `x`, set modification times (the default).
//! - `R`: with `x`, keep a leading `/` or `#` in names rather than
//!   extracting relative to the current directory.
//! - `p`: write POSIX ustar (the only format written).
//!
//! `x` sets modes from the archive. Names given to `t` and `x` select
//! those entries and anything under them.

use std::fs::File;
use std::io::{self, Read, Write};

use archive::fs::{Extract, Put, selected, warn};
use archive::ustar::{Kind, Reader, Writer};
use archive::{gz, mode_string};
use purgatorio::abi::mode;
use purgatorio::{Context, Error, daytime};

const USAGE: &str = "usage: tar {ctx}[fkmpRTvz] [archive] [file ...]";

#[derive(Default)]
struct Opts {
    func: char,
    file: Option<String>,
    verbose: bool,
    gzip: bool,
    keep: bool,
    no_mtime: bool,
    rooted: bool,
}

fn parse(args: &[String]) -> Result<(Opts, Vec<String>), Error> {
    let Some(key) = args.get(1) else {
        return Err(Error::from(USAGE));
    };
    let mut o = Opts::default();
    let mut rest = args[2..].iter();
    for c in key.trim_start_matches('-').chars() {
        match c {
            'c' | 't' | 'x' if o.func == '\0' => o.func = c,
            'f' => o.file = Some(rest.next().ok_or(USAGE)?.clone()),
            'v' => o.verbose = true,
            'z' => o.gzip = true,
            'k' => o.keep = true,
            'm' => o.no_mtime = true,
            'T' => o.no_mtime = false,
            'R' => o.rooted = true,
            'p' => {}
            _ => return Err(Error::from(USAGE)),
        }
    }
    if o.func == '\0' {
        return Err(Error::from(USAGE));
    }
    Ok((o, rest.cloned().collect()))
}

/// Reads the whole archive, ungzipping it if need be.
fn read_archive(o: &Opts) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    match &o.file {
        Some(f) if f != "-" => {
            File::open(f)
                .and_then(|mut f| f.read_to_end(&mut data))
                .map_err(|_| Error(format!("{f}: {}", purgatorio::errstr())))?;
        }
        _ => {
            io::stdin().lock().read_to_end(&mut data)?;
        }
    }
    if gz::is_gzip(&data) {
        data = gz::decompress(&data).map_err(Error::from)?;
    }
    Ok(data)
}

fn create(o: &Opts, files: &[String]) -> Result<bool, Error> {
    if files.is_empty() {
        return Err(Error::from(USAGE));
    }
    let mut out: Box<dyn Write> = match &o.file {
        Some(f) if f != "-" => {
            let fd = purgatorio::create(f, mode::OWRITE, 0o666)
                .map_err(|e| Error(format!("{f}: {e}")))?;
            Box::new(io::BufWriter::new(File::from(fd)))
        }
        _ => Box::new(io::BufWriter::new(io::stdout().lock())),
    };
    let mut put = Put {
        prog: "tar",
        recurse: true,
        verbose: o.verbose,
        failed: false,
    };
    if o.gzip {
        let mut w = Writer::new(Vec::new());
        for f in files {
            put.add(&mut w, f)?;
        }
        let tar = w.finish()?;
        out.write_all(&gz::compress(&tar, gz::DEFAULT_LEVEL))?;
    } else {
        let mut w = Writer::new(&mut out);
        for f in files {
            put.add(&mut w, f)?;
        }
        w.finish()?;
    }
    out.flush()?;
    Ok(!put.failed)
}

fn list(o: &Opts, names: &[String]) -> Result<bool, Error> {
    let data = read_archive(o)?;
    let mut r = Reader::new(&data[..]);
    let mut out = io::BufWriter::new(io::stdout().lock());
    let now = daytime::now();
    while let Some(h) = r.next_entry()? {
        if !selected(&h.name, names) {
            continue;
        }
        if o.verbose {
            let mut m = h.mode;
            if h.kind == Kind::Dir {
                m |= purgatorio::DMDIR;
            }
            let mut name = h.name.clone();
            if h.kind == Kind::Link {
                name = format!("{name} -> {}", h.link);
            }
            writeln!(
                out,
                "{} {:8} {} {name}",
                mode_string(m),
                h.size,
                daytime::filet(now, h.mtime as i64)
            )?;
        } else {
            writeln!(out, "{}", h.name)?;
        }
    }
    out.flush()?;
    Ok(true)
}

fn extract(o: &Opts, names: &[String]) -> Result<bool, Error> {
    let data = read_archive(o)?;
    let mut r = Reader::new(&data[..]);
    let mut x = Extract::new("tar");
    x.keep = o.keep;
    x.rooted = o.rooted;
    x.mtimes = !o.no_mtime;
    x.verbose = o.verbose;
    let res = (|| {
        while let Some(h) = r.next_entry()? {
            if selected(&h.name, names) {
                x.entry(&mut r, &h)?;
            }
        }
        Ok::<(), String>(())
    })();
    x.finish();
    res?;
    Ok(!x.failed)
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (o, files) = parse(&args)?;
    let ok = match o.func {
        'c' => create(&o, &files),
        't' => list(&o, &files),
        _ => extract(&o, &files),
    };
    match ok {
        Ok(true) => Ok(()),
        Ok(false) => purgatorio::exits("fail:errors"),
        Err(e) => {
            warn("tar", &e.0);
            purgatorio::exits(&format!("fail:{e}"))
        }
    }
}
