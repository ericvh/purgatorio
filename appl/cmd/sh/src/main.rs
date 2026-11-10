//! sh [-il] [-c command | file] [arg ...]: the command interpreter, a
//! subset of Inferno's sh.
//!
//! Supported: simple commands, pipelines (`|`), `;`, `&`, `&&`, `||`, `!`,
//! `{` blocks `}`, redirections (`<`, `>`, `>>`, `>[n]`, `>[n=m]`,
//! `>[n=]`), `'quoting'`, `^` concatenation, list variables (`x=(a b)`,
//! `$x`, `$#x`, `$"x`, `$*`, `$1`, `$status`, `$apid`), `` `{command} ``
//! substitution, `${expr ...}` and comments. Variables are exported through
//! `/env` unless declared with `:=`. Builtins: `cd`, `exit`, `run`, `wait`,
//! `load`. `load std` adds `if`, `while`, `for`, `and`, `or`, `not`, `~`,
//! `fn`, `raise` and `${hd}`, `${tl}`, `${index}`, `${quote}` and
//! `${unquote}`. `load expr` and `load mpexpr` add `${expr}` and `ntest`.
//! `load string` adds the sh-string(1) builtins and `load regex` adds
//! `match` and `${re}`. `load arg`, `csv`, `echo`, `file2chan`, `mload`,
//! `sexprs` and `test` add the builtins of their sh-*(1) pages. All of
//! these are compiled in; any other module name loads a reactor from
//! `/bin/lib`.
//!
//! With `-l`, `/lib/sh/profile` runs first. Without `-c` or a file,
//! commands are read from standard input, with a `; ` prompt if it is the
//! console or `-i` is given. Two options are for sh's own sub-shells: `-P
//! prelude` runs `prelude` (the parent's `load`s and `fn`s) quietly first,
//! and `-F file readcmd writecmd [closecmd]` serves a `file2chan` file.

mod csv;
mod exec;
mod f2c;
mod input;
mod lex;
mod modules;
mod num;
mod parse;
mod regex;
mod sexp;
mod shmods;
mod string;

use std::io::Read;

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

use exec::{Shell, Stop};
use lex::SyntaxError;

const PROFILE: &str = "/lib/sh/profile";

fn finish(sh: &Shell, stop: Option<Stop>) -> ! {
    let status = match stop {
        Some(Stop::Exit(s) | Stop::Raise(s)) => s,
        None => sh.status.clone(),
    };
    purgatorio::exits(&status)
}

/// Reads commands from fd 0, a line at a time so that commands that read
/// standard input themselves see the rest of it.
fn interactive(sh: &mut Shell, prompt: bool) -> Option<Stop> {
    let mut stdin = std::io::stdin();
    let mut pending = String::new();
    let mut buf = Vec::new();
    loop {
        let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
            if prompt {
                let p = if pending.is_empty() { "; " } else { "\t" };
                print!("{p}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            let mut chunk = [0u8; 4096];
            match stdin.read(&mut chunk) {
                Ok(0) | Err(_) => {
                    if !pending.is_empty() || !buf.is_empty() {
                        pending.push_str(&String::from_utf8_lossy(&buf));
                        if let Err(Stop::Exit(s)) = sh.run_src(&pending) {
                            return Some(Stop::Exit(s));
                        }
                    }
                    return None;
                }
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            continue;
        };
        let line: Vec<u8> = buf.drain(..=nl).collect();
        pending.push_str(&String::from_utf8_lossy(&line));
        match parse::parse(&pending) {
            Err(SyntaxError::Incomplete) => continue,
            _ => {
                let src = std::mem::take(&mut pending);
                if let Err(stop) = sh.run_src(&src) {
                    match stop {
                        Stop::Raise(s) => sh.status = s,
                        other => return Some(other),
                    }
                }
            }
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "sh [-il] [-c command | file] [arg ...]");
    let (mut login, mut force_prompt, mut command) = (false, false, None);
    let (mut prelude, mut serve) = (None, false);
    while let Some(c) = arg.opt() {
        match c {
            'l' => login = true,
            'i' => force_prompt = true,
            'c' => command = Some(arg.earg()?),
            'P' => prelude = Some(arg.earg()?),
            'F' => serve = true,
            _ => return Err(arg.usage()),
        }
    }
    let mut rest = arg.argv();
    if serve {
        let mut sh = Shell::new("sh", Vec::new());
        if let Some(src) = prelude {
            sh.quiet = true;
            let _ = sh.run_src(&src);
            sh.quiet = false;
        }
        // As in Inferno, a file that cannot be made is reported by the
        // status of file2chan only.
        if let Err(e) = sh.serve_file2chan(&rest) {
            purgatorio::exits(&e.0);
        }
        return Ok(());
    }
    let file = if command.is_none() && !rest.is_empty() {
        Some(rest.remove(0))
    } else {
        None
    };
    let mut sh = Shell::new(file.as_deref().unwrap_or("sh"), rest);
    if let Some(src) = prelude {
        sh.quiet = true;
        let r = sh.run_src(&src);
        sh.quiet = false;
        if let Err(stop) = r {
            finish(&sh, Some(stop));
        }
    }
    if login && purgatorio::stat(PROFILE).is_ok() {
        let src = std::fs::read_to_string(PROFILE)?;
        if let Err(stop) = sh.run_src(&src) {
            finish(&sh, Some(stop));
        }
    }
    let stop = if let Some(src) = command {
        sh.run_src(&src).err()
    } else if let Some(file) = file {
        let src = std::fs::read_to_string(&file)
            .map_err(|e| Error(format!("{file}: {}", Error::from(e))))?;
        sh.run_src(&src).err()
    } else {
        let prompt = force_prompt || purgatorio::fd2path(0).is_ok_and(|p| p == "/dev/cons");
        interactive(&mut sh, prompt)
    };
    finish(&sh, stop)
}
