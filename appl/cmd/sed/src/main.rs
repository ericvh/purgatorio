//! sed [-ngd] [-e script] [-f sfile] [script] [file ...]: the stream
//! editor of sed(1). Each `-e` and `-f` adds lines to the script; without
//! them the first argument is the script. The regular expressions are
//! those of regexp(6), with `( )` grouping; in an `s` replacement `&` is
//! the match and `\1` to `\9` are subexpressions. `-n` turns off the
//! automatic print, `-g` makes every `s` global and `-d` is accepted and
//! ignored.
//!
//! Errors print `sed: msg` and exit with `fail:error`; a bad command line
//! exits with `fail:usage`.

mod parse;
mod run;

use std::io::Write;

use purgatorio::{Context, Error};

const USAGE: &str = "usage: sed [-ngd] [-e expr] [-f file] [expr] [file...]";

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut quiet = false;
    let mut global = false;
    let mut pieces: Vec<String> = Vec::new();
    let mut i = 1;
    while let Some(a) = args.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        if !a.starts_with('-') || a.len() == 1 {
            break;
        }
        i += 1;
        for (at, c) in a[1..].char_indices() {
            match c {
                'n' => quiet = true,
                'g' => global = true,
                'd' => {}
                'e' | 'f' => {
                    // The argument is the rest of this word or the next.
                    let rest = &a[1 + at + 1..];
                    let val = if rest.is_empty() {
                        i += 1;
                        args.get(i - 1).cloned().unwrap_or_else(|| usage())
                    } else {
                        rest.to_string()
                    };
                    if c == 'e' {
                        pieces.push(val);
                    } else {
                        match std::fs::read_to_string(&val) {
                            Ok(t) => pieces.push(t.strip_suffix('\n').unwrap_or(&t).to_string()),
                            Err(e) => fatal(&format!("couldn't open '{val}': {}", Error::from(e))),
                        }
                    }
                    break;
                }
                _ => usage(),
            }
        }
    }
    if pieces.is_empty() {
        match args.get(i) {
            Some(s) => {
                pieces.push(s.clone());
                i += 1;
            }
            None => fatal("missing pattern"),
        }
    }
    let script = parse::compile(&pieces.join("\n"), global).unwrap_or_else(|e| fatal(&e));
    let mut wfiles: Vec<Box<dyn Write>> = Vec::new();
    for name in &script.wfiles {
        match std::fs::File::create(name) {
            Ok(f) => wfiles.push(Box::new(std::io::BufWriter::new(f))),
            Err(_) => fatal(&format!("can't create output file: '{name}'")),
        }
    }
    let files = args[i..].to_vec();
    let mut next = 0;
    let mut stdin_done = false;
    let mut input = run::Input::new(move || -> Result<Option<run::Reader<'static>>, String> {
        if files.is_empty() {
            if stdin_done {
                return Ok(None);
            }
            stdin_done = true;
            return Ok(Some(Box::new(std::io::stdin().lock())));
        }
        let Some(f) = files.get(next) else {
            return Ok(None);
        };
        next += 1;
        match std::fs::File::open(f) {
            Ok(file) => Ok(Some(Box::new(std::io::BufReader::new(file)))),
            Err(_) => Err(format!("can't open '{f}'")),
        }
    });
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    let opts = run::Options { quiet };
    if let Err(e) = run::run(&script, &opts, &mut input, &mut out, &mut wfiles) {
        drop(out);
        fatal(&e);
    }
    Ok(())
}

fn fatal(msg: &str) -> ! {
    eprintln!("sed: {msg}");
    purgatorio::exits("fail:error");
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("fail:usage");
}
