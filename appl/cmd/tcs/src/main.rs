//! tcs [-f ics] [-t ocs] [file ...]: convert text from character set `ics`
//! to `ocs` (both UTF-8 by default). `tcs -l [-v] [csname]` lists the
//! known character sets, with `-v` their descriptions, or the aliases of
//! `csname`. The sets and their aliases are listed in
//! `/lib/convcs/charsets`; the converters are built in.
//!
//! Input that is not valid in `ics` becomes U+FFFD; a character that
//! `ocs` cannot represent becomes `?`.

use std::io::{self, Read, Write};

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

const CHARSETS: &str = "/lib/convcs/charsets";

struct Charset {
    names: Vec<String>,
    conv: String,
    desc: String,
}

fn load() -> Result<Vec<Charset>, Error> {
    let text = std::fs::read_to_string(CHARSETS)
        .map_err(|_| Error(format!("cannot open {CHARSETS}: {}", purgatorio::errstr())))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.splitn(3, ':');
        let names: Vec<String> = f
            .next()
            .unwrap_or("")
            .split_whitespace()
            .map(String::from)
            .collect();
        let conv = f.next().unwrap_or("").trim();
        let desc = f.next().unwrap_or("").trim();
        if names.is_empty() || conv.is_empty() {
            continue;
        }
        out.push(Charset {
            names,
            conv: String::from(conv),
            desc: String::from(desc),
        });
    }
    Ok(out)
}

fn find<'a>(sets: &'a [Charset], name: &str) -> Result<&'a Charset, Error> {
    let lower = name.to_lowercase();
    sets.iter()
        .find(|c| c.names.iter().any(|n| n.to_lowercase() == lower))
        .ok_or_else(|| Error(format!("{name}: unknown character set")))
}

fn utf16(data: &[u8], big: bool) -> String {
    let units = data.chunks(2).map(|p| match p {
        [a, b] if big => u16::from_be_bytes([*a, *b]),
        [a, b] => u16::from_le_bytes([*a, *b]),
        _ => 0xfffd,
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn decode(conv: &str, data: &[u8]) -> Result<String, Error> {
    Ok(match conv {
        "utf8" => String::from_utf8_lossy(data).into_owned(),
        "ascii" => data
            .iter()
            .map(|&b| {
                if b < 0x80 {
                    b as char
                } else {
                    char::REPLACEMENT_CHARACTER
                }
            })
            .collect(),
        "latin1" => data.iter().map(|&b| b as char).collect(),
        "utf16" => match data {
            [0xfe, 0xff, rest @ ..] => utf16(rest, true),
            [0xff, 0xfe, rest @ ..] => utf16(rest, false),
            _ => utf16(data, true),
        },
        "utf16be" => utf16(data, true),
        "utf16le" => utf16(data, false),
        _ => return Err(Error(format!("{conv}: unknown converter"))),
    })
}

fn encode(conv: &str, s: &str) -> Result<Vec<u8>, Error> {
    let narrow = |max: u32| -> Vec<u8> {
        s.chars()
            .map(|c| {
                if (c as u32) <= max {
                    c as u32 as u8
                } else {
                    b'?'
                }
            })
            .collect()
    };
    Ok(match conv {
        "utf8" => s.as_bytes().to_vec(),
        "ascii" => narrow(0x7f),
        "latin1" => narrow(0xff),
        "utf16" | "utf16be" => {
            let mut out = if conv == "utf16" {
                vec![0xfe, 0xff]
            } else {
                Vec::new()
            };
            out.extend(s.encode_utf16().flat_map(u16::to_be_bytes));
            out
        }
        "utf16le" => s.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        _ => return Err(Error(format!("{conv}: unknown converter"))),
    })
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "tcs -l [-v] [cs] | tcs [-f ics] [-t ocs] [file ...]");
    let (mut list, mut verbose) = (false, false);
    let (mut ics, mut ocs) = (String::from("utf8"), String::from("utf8"));
    while let Some(o) = arg.opt() {
        match o {
            'l' => list = true,
            'v' => verbose = true,
            'f' => ics = arg.earg()?,
            't' => ocs = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let files = arg.argv();
    let sets = load()?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if list {
        match files.as_slice() {
            [] if verbose => {
                for c in &sets {
                    writeln!(out, "{}\t{}", c.names[0], c.desc)?;
                }
            }
            [] => {
                let names: Vec<&str> = sets.iter().map(|c| c.names[0].as_str()).collect();
                writeln!(out, "{}", names.join(" "))?;
            }
            [cs] => {
                let c = find(&sets, cs)?;
                if verbose {
                    writeln!(out, "{}", c.desc)?;
                }
                writeln!(out, "{}", c.names.join(" "))?;
            }
            _ => return Err(usage),
        }
        out.flush()?;
        return Ok(());
    }
    let from = &find(&sets, &ics)?.conv;
    let to = &find(&sets, &ocs)?.conv;
    let mut data = Vec::new();
    if files.is_empty() {
        io::stdin().lock().read_to_end(&mut data)?;
    }
    for f in &files {
        let mut file = std::fs::File::open(f)
            .map_err(|_| Error(format!("cannot open {f}: {}", purgatorio::errstr())))?;
        file.read_to_end(&mut data)?;
    }
    out.write_all(&encode(to, &decode(from, &data)?)?)?;
    out.flush()?;
    Ok(())
}
