//! mash [-denx] [-c command] [file [arg ...]]: the programmable shell of
//! mash(1), with the make builtins of mash-make(1).
//!
//! Commands end with `;` or `&` (a newline ends one only when typed at the
//! console). The language has simple commands, pipelines, `<` `>` `>>` and
//! `<>` redirections, `'quoting'`, list variables (`x = a b`, `x := a b`
//! for a local), `$x`, `$"x`, free and explicit `^` concatenation, the
//! generators `` `{...} ``, `"{...}`, `<{...}` and `>{...}`, `(expr)`
//! expressions with `hd`, `tl`, `len`, `!`, `^`, `::`, `==`, `!=` and the
//! regular-expression match `~`, `if`/`else`, `for`, `while`, `case`, `fn`,
//! `{...}` and `@{...}` groups, and file name patterns.
//!
//! The builtins are `env`, `eval`, `exit`, `load`, `prompt`, `quote`,
//! `run`, `status`, `time` and `whatis`, and `cd`, which Purgatorio's
//! commands cannot do for mash. `load make` adds `make`, `depends`,
//! `match` and `rules`; `load history` serves `/chan/history` and
//! `/chan/mash`; `load file` loads a reactor module of builtins. The Tk
//! builtins (`load tk`) wait for Purgatorio's Tk.
//!
//! With no arguments, mash runs `/lib/mashinit` first, then reads the
//! console with a `mash% ` prompt, or standard input as a file.

mod ast;
mod exec;
mod history;
mod lex;
mod make;
mod parse;

use std::io::Read;

use purgatorio::{Context, Error};

use exec::{Io, Shell, Stop};

const USAGE: &str = "usage: mash [-denx] [-c command] [src [args]]";
const PROFILE: &str = "/lib/mashinit";

fn finish(sh: &Shell, stop: Option<Stop>) -> ! {
    if let Some(h) = &sh.history {
        h.stop();
    }
    match stop {
        Some(Stop::Fail(s)) => purgatorio::exits(&s),
        _ => purgatorio::exits(""),
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    purgatorio::exits("fail: mash usage")
}

/// Reads lines from the console, each a complete input. A line ending in
/// a backslash continues on the next.
fn interactive(sh: &mut Shell) -> Option<Stop> {
    let io = Io::std();
    let mut stdin = std::io::stdin();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        io.out.print(&sh.prompt);
        let pending = sh.history.as_ref().map(|h| h.pending()).unwrap_or_default();
        if !pending.is_empty() {
            io.out.print(&pending);
            if let Some(stop) = run_line(sh, &io, pending) {
                return Some(stop);
            }
            continue;
        }
        let mut line = read_line(&mut stdin, &mut buf)?;
        while line.ends_with("\\\n") {
            line.truncate(line.len() - 2);
            line.push_str("  ");
            io.out.print(&sh.contin);
            match read_line(&mut stdin, &mut buf) {
                Some(l) => line.push_str(&l),
                None => break,
            }
        }
        if let Some(stop) = run_line(sh, &io, line) {
            return Some(stop);
        }
    }
}

/// The next line of fd 0 with its newline, or what is left at the end of
/// the input. It reads no further than the line, so commands that read
/// standard input see the rest.
fn read_line(stdin: &mut std::io::Stdin, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let l: Vec<u8> = buf.drain(..=nl).collect();
            return Some(String::from_utf8_lossy(&l).into_owned());
        }
        let mut chunk = [0u8; 4096];
        match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => {
                if buf.is_empty() {
                    return None;
                }
                let l = std::mem::take(buf);
                return Some(String::from_utf8_lossy(&l).into_owned());
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn run_line(sh: &mut Shell, io: &Io, line: String) -> Option<Stop> {
    let r = sh.run_source(&line, None, io);
    if let Some(h) = &sh.history {
        h.record(&line);
    }
    match r {
        Err(Stop::Exit) => Some(Stop::Exit),
        _ => None,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some("--history") => return history::serve_history(),
        Some("--serve") => return history::serve_mash(),
        _ => {}
    }
    let mut sh = Shell::new();
    let mut rest: Vec<String> = args.into_iter().skip(1).collect();
    let mut command = None;
    while let Some(s) = rest.first().filter(|s| s.starts_with('-')).cloned() {
        rest.remove(0);
        if s == "--" {
            break;
        }
        let flags: Vec<char> = s.chars().skip(1).collect();
        for (i, c) in flags.iter().enumerate() {
            match c {
                'c' => {
                    let tail: String = flags[i + 1..].iter().collect();
                    if !tail.is_empty() {
                        if !rest.is_empty() {
                            usage();
                        }
                        command = Some(tail);
                    } else {
                        if rest.len() != 1 {
                            usage();
                        }
                        command = Some(rest.remove(0));
                    }
                    break;
                }
                'd' => sh.flags.dump = true,
                'e' => sh.flags.raise = true,
                'n' => sh.flags.noexec = true,
                'x' => sh.flags.echo = true,
                _ => usage(),
            }
        }
        if command.is_some() {
            break;
        }
    }
    let io = Io::std();
    if command.is_none() && rest.is_empty() && purgatorio::stat(PROFILE).is_ok() {
        sh.globals
            .insert(String::from("mashinit"), vec![String::from("true")]);
        if let Ok(src) = std::fs::read_to_string(PROFILE)
            && let Err(stop @ (Stop::Exit | Stop::Fail(_))) =
                sh.run_source(&src, Some(PROFILE), &io)
        {
            finish(&sh, Some(stop));
        }
    }
    sh.globals.insert(String::from("mashinit"), Vec::new());
    let stop = if let Some(src) = command {
        sh.run_source(&src, Some("<string>"), &io).err()
    } else if !rest.is_empty() {
        let file = rest.remove(0);
        let src = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not open {file}: {}", Error::from(e));
                purgatorio::exits("fail: mash system error");
            }
        };
        sh.globals.insert(String::from("args"), rest);
        sh.run_source(&src, Some(&file), &io).err()
    } else if purgatorio::fd2path(0).is_ok_and(|p| p == "/dev/cons") {
        interactive(&mut sh)
    } else {
        let mut src = String::new();
        if let Err(e) = std::io::stdin().read_to_string(&mut src) {
            eprintln!("error reading <stdin>: {}", Error::from(e));
        }
        sh.run_source(&src, Some("<stdin>"), &io).err()
    };
    finish(&sh, stop)
}
