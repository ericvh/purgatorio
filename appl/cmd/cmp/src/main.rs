//! cmp [-lsL] file1 file2 [offset1 [offset2]]: compare two files, as
//! Inferno's cmp(1). With no difference there is no output. Otherwise cmp
//! prints `file1 file2 differ: char N` (with ` line L` under `-L`) and
//! fails with `differ`; `-l` prints each difference as the byte number
//! and the two bytes in hex; `-s` prints nothing for a difference. When
//! one file ends first it prints `EOF on file after N bytes` and fails
//! with `EOF`. Offsets start the comparison there in each file: `0x` is
//! hexadecimal, a leading `0` octal, anything else decimal. A file named
//! `-` is standard input (an addition: Inferno's cmp opens a file named
//! `-`).

use purgatorio::{eprintln, println};
use std::io::{Read, Seek, SeekFrom};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const USAGE: &str = "Usage: cmp [-lsL] file1 file2 [offset1 [offset2] ]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, USAGE);
    let (mut lflag, mut sflag, mut big_l) = (false, false, false);
    while let Some(c) = arg.opt() {
        match c {
            'l' => lflag = true,
            's' => sflag = true,
            'L' => big_l = true,
            _ => usage(),
        }
    }
    let argv = arg.argv();
    if !(2..=4).contains(&argv.len()) {
        usage();
    }
    let (name1, name2) = (&argv[0], &argv[1]);
    let a = read(name1, argv.get(2), 1);
    let b = read(name2, argv.get(3), 2);
    let (mut line, mut differ) = (1u64, false);
    let n = a.len().min(b.len());
    for i in 0..n {
        if big_l && a[i] == b'\n' {
            line += 1;
        }
        if a[i] != b[i] {
            let at = i + 1;
            if !lflag {
                if !sflag {
                    if big_l {
                        println!("{name1} {name2} differ: char {at} line {line}");
                    } else {
                        println!("{name1} {name2} differ: char {at}");
                    }
                }
                purgatorio::exits("differ");
            }
            println!("{at:>6} 0x{:02x} 0x{:02x}", a[i], b[i]);
            differ = true;
        }
    }
    // As Inferno's: the end of one file is reported even under -s.
    if a.len() != b.len() {
        let short = if a.len() > b.len() { name2 } else { name1 };
        println!("EOF on {short} after {n} bytes");
        purgatorio::exits("EOF");
    }
    if differ {
        purgatorio::exits("differ");
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("usage");
}

/// An offset as cmp(1) writes it: `0x` hex, a leading `0` octal, else
/// decimal. What does not parse is 0, as Inferno's conversion with
/// `big` makes it.
fn offset(s: &str) -> u64 {
    let n = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else if s.len() > 1 && s.starts_with('0') {
        u64::from_str_radix(&s[1..], 8).ok()
    } else {
        s.parse().ok()
    };
    n.unwrap_or(0)
}

/// The contents of file `which` (1 or 2) from `off`, or the reason it
/// cannot be had as Inferno's cmp gives it.
fn read(path: &str, off: Option<&String>, which: u8) -> Vec<u8> {
    let mut data = Vec::new();
    if path == "-" {
        if purgatorio::io::stdin().read_to_end(&mut data).is_err() {
            eprintln!("cmp: can't open {path}: {}", Error::last());
            purgatorio::exits("open");
        }
        let skip = off.map_or(0, |o| offset(o)) as usize;
        return data.split_off(skip.min(data.len()));
    }
    let mut f = purgatorio::fs::File::open(path).unwrap_or_else(|e| {
        eprintln!("cmp: can't open {path}: {}", Error::from(e));
        purgatorio::exits("open");
    });
    if let Some(o) = off
        && f.seek(SeekFrom::Start(offset(o))).is_err()
    {
        eprintln!("cmp: seek by offset{which} failed: {}", Error::last());
        purgatorio::exits(&format!("seek {which}"));
    }
    if f.read_to_end(&mut data).is_err() {
        println!("error on {path} after 0 bytes");
        purgatorio::exits("read error");
    }
    data
}
