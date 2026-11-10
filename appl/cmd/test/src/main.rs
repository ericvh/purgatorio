//! test expression: exit with an empty status when `expression` is true,
//! `fail:false` when it is false. The grammar is the traditional one:
//! `-o` (or) binds looser than `-a` (and), then `!` and parentheses.
//!
//! Primaries: `-d -e -f -r -w -x -s -A -L -T file` (directory, exists,
//! plain file, readable, writable, executable, non-empty, append only,
//! exclusive, temporary); `-t [fd]` (fd, default 1, is the console);
//! `-n s` and `-z s` (non-empty, empty); `s1 = s2` and `s1 != s2`;
//! `n1 -eq -ne -gt -lt -ge -le n2` on decimal integers; `f1 -nt f2` and
//! `f1 -ot f2` (newer or older modification time); `file -older time`,
//! where `time` is seconds or a list such as `1d12h` of `y M d h m s`
//! units; and a lone string, which is true when it is not empty.

use std::os::fd::AsRawFd;

use purgatorio::abi::mode;
use purgatorio::{Context, DMAPPEND, DMDIR, DMEXCL, Dir, Error};

const DMTMP: u32 = 0x0400_0000;

struct Syntax(String);

type R<T> = Result<T, Syntax>;

struct Test {
    args: Vec<String>,
    pos: usize,
}

fn is_int(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn stat(f: &str) -> Option<Dir> {
    purgatorio::stat(f).ok()
}

fn has_mode(f: &str, m: u32) -> bool {
    stat(f).is_some_and(|d| d.mode & m != 0)
}

fn is_cons(fd: i32) -> bool {
    match purgatorio::fd2path(fd) {
        Ok(s) => s == "#c/cons" || s.ends_with("/dev/cons"),
        Err(_) => false,
    }
}

fn mtimes(a: &str, b: &str) -> Option<(u32, u32)> {
    Some((stat(a)?.mtime, stat(b)?.mtime))
}

impl Test {
    fn next(&mut self) -> R<String> {
        let a = self
            .args
            .get(self.pos)
            .cloned()
            .ok_or_else(|| Syntax(String::from("argument expected")))?;
        self.pos += 1;
        Ok(a)
    }

    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.peek() == Some(s) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn or(&mut self) -> R<bool> {
        let a = self.and()?;
        if self.eat("-o") {
            let b = self.or()?;
            return Ok(a || b);
        }
        Ok(a)
    }

    fn and(&mut self) -> R<bool> {
        let a = self.not()?;
        if self.eat("-a") {
            let b = self.and()?;
            return Ok(a && b);
        }
        Ok(a)
    }

    fn not(&mut self) -> R<bool> {
        if self.eat("!") {
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> R<bool> {
        let a = self.next()?;
        Ok(match a.as_str() {
            "(" => {
                let v = self.or()?;
                if self.next()? != ")" {
                    return Err(Syntax(String::from(") expected")));
                }
                v
            }
            "-A" => has_mode(&self.next()?, DMAPPEND),
            "-L" => has_mode(&self.next()?, DMEXCL),
            "-T" => has_mode(&self.next()?, DMTMP),
            "-d" => has_mode(&self.next()?, DMDIR),
            "-f" => stat(&self.next()?).is_some_and(|d| d.mode & DMDIR == 0),
            "-e" => stat(&self.next()?).is_some(),
            "-s" => stat(&self.next()?).is_some_and(|d| d.length > 0),
            "-r" => purgatorio::open(&self.next()?, mode::OREAD).is_ok(),
            "-w" => purgatorio::open(&self.next()?, mode::OWRITE).is_ok(),
            "-x" => match purgatorio::open(&self.next()?, mode::OREAD) {
                Ok(fd) => purgatorio::fstat(fd.as_raw_fd()).is_ok_and(|d| d.mode & 0o111 != 0),
                Err(_) => false,
            },
            "-t" => {
                if self.peek().is_some_and(is_int) {
                    let n = self.next()?;
                    is_cons(n.parse().unwrap_or(-1))
                } else {
                    is_cons(1)
                }
            }
            "-n" => !self.next()?.is_empty(),
            "-z" => self.next()?.is_empty(),
            _ => return self.binary(a),
        })
    }

    fn binary(&mut self, a: String) -> R<bool> {
        let Some(op) = self.peek().map(String::from) else {
            return Ok(!a.is_empty());
        };
        self.pos += 1;
        match op.as_str() {
            "=" => return Ok(self.next()? == a),
            "!=" => return Ok(self.next()? != a),
            "-older" => return older(&a, &self.next()?),
            "-ot" => return Ok(mtimes(&a, &self.next()?).is_some_and(|(x, y)| x < y)),
            "-nt" => return Ok(mtimes(&a, &self.next()?).is_some_and(|(x, y)| x > y)),
            _ => {}
        }
        if !is_int(&a) {
            // A string followed by something that is not an operator:
            // the string alone is the primary.
            self.pos -= 1;
            return Ok(!a.is_empty());
        }
        if self.peek().is_some_and(is_int) {
            let x: i128 = a.parse().unwrap_or(0);
            let y: i128 = self.next()?.parse().unwrap_or(0);
            let v = match op.as_str() {
                "-eq" => Some(x == y),
                "-ne" => Some(x != y),
                "-gt" => Some(x > y),
                "-lt" => Some(x < y),
                "-ge" => Some(x >= y),
                "-le" => Some(x <= y),
                _ => None,
            };
            if let Some(v) = v {
                return Ok(v);
            }
        }
        Err(Syntax(format!("unknown operator {op}")))
    }
}

/// `file -older t`: the file's modification time plus `t` is before now.
fn older(file: &str, t: &str) -> R<bool> {
    let bad = || Syntax(format!("bad time syntax, {t}"));
    let mut n: i64 = 0;
    let mut rest = t;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(bad());
        }
        let m: i64 = rest[..digits].parse().map_err(|_| bad())?;
        rest = &rest[digits..];
        let Some(unit) = rest.chars().next() else {
            n = m;
            break;
        };
        rest = &rest[unit.len_utf8()..];
        n += m * match unit {
            'y' => 12 * 30 * 24 * 3600,
            'M' => 30 * 24 * 3600,
            'd' => 24 * 3600,
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return Err(bad()),
        };
    }
    let Some(d) = stat(file) else {
        return Ok(false);
    };
    Ok(i64::from(d.mtime) + n < purgatorio::daytime::now())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        purgatorio::exits("fail:usage");
    }
    let mut t = Test { args, pos: 1 };
    match t.or() {
        Ok(true) => Ok(()),
        Ok(false) => purgatorio::exits("fail:false"),
        Err(Syntax(s)) => {
            eprintln!("test: bad syntax: {s}");
            purgatorio::exits("fail:bad syntax");
        }
    }
}
