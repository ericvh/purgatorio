//! sed 's/old/new/[g]' [file...]: a small substitute command.

use std::io::{BufRead, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let script = args
        .get(1)
        .ok_or_else(|| Error::from("usage: sed script [file...]"))?;
    let (old, new, global) = parse(script)?;
    let files = &args[2..];
    let run = |input: &mut dyn BufRead| -> Result<(), Error> {
        let mut out = std::io::stdout().lock();
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                break;
            }
            let nl = line.ends_with('\n');
            let body = if nl {
                &line[..line.len() - 1]
            } else {
                line.as_str()
            };
            let replaced = subst(body, &old, &new, global);
            out.write_all(replaced.as_bytes())?;
            if nl {
                out.write_all(b"\n")?;
            }
        }
        Ok(())
    };
    if files.is_empty() {
        run(&mut std::io::stdin().lock())
    } else {
        for f in files {
            let file = std::fs::File::open(f).map_err(|_| Error::last())?;
            run(&mut std::io::BufReader::new(file))?;
        }
        Ok(())
    }
}

fn parse(script: &str) -> Result<(String, String, bool), Error> {
    let mut c = script.chars();
    if c.next() != Some('s') {
        return Err(Error::from("only s/// scripts"));
    }
    let delim = c.next().ok_or_else(|| Error::from("only s/// scripts"))?;
    let rest: String = c.collect();
    let mut parts = rest.split(delim);
    let old = parts.next().unwrap_or("").to_string();
    let new = parts.next().unwrap_or("").to_string();
    let flags = parts.next().unwrap_or("");
    Ok((old, new, flags.contains('g')))
}

fn subst(line: &str, old: &str, new: &str, global: bool) -> String {
    if old.is_empty() {
        return String::from(line);
    }
    if !global {
        return line.replacen(old, new, 1);
    }
    line.replace(old, new)
}
