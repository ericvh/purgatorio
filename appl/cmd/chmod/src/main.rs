//! chmod mode file ...: change file permissions. `mode` is octal (an
//! optional `8r` prefix is ignored) or symbolic, `[ugoa][+-=][rwxalt]`:
//! who (user, group, other; `a` or nothing for all), what to do (add,
//! remove, set), and which bits (read, write, execute, append only,
//! exclusive, temporary).

use purgatorio::{Context, DMAPPEND, DMDIR, DMEXCL, Dir, Error};

const DMTMP: u32 = 0x0400_0000;
const SPECIAL: u32 = DMAPPEND | DMEXCL | DMTMP;

/// A parsed mode: either absolute permission bits or a symbolic change.
enum Mode {
    Octal(u32),
    Sym { who: u32, op: char, bits: u32 },
}

fn parse(s: &str) -> Option<Mode> {
    let o = s.strip_prefix("8r").unwrap_or(s);
    if !o.is_empty() && o.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return u32::from_str_radix(o, 8)
            .ok()
            .filter(|m| *m <= 0o777)
            .map(Mode::Octal);
    }
    let mut chars = s.chars().peekable();
    let mut who = 0;
    while let Some(&c) = chars.peek() {
        who |= match c {
            'u' => 0o700,
            'g' => 0o070,
            'o' => 0o007,
            'a' => 0o777,
            _ => break,
        };
        chars.next();
    }
    if who == 0 {
        who = 0o777;
    }
    let op = chars.next().filter(|c| matches!(c, '+' | '-' | '='))?;
    let mut bits = 0;
    for c in chars {
        bits |= match c {
            'r' => 0o444 & who,
            'w' => 0o222 & who,
            'x' => 0o111 & who,
            'a' => DMAPPEND,
            'l' => DMEXCL,
            't' => DMTMP,
            _ => return None,
        };
    }
    Some(Mode::Sym { who, op, bits })
}

fn apply(m: &Mode, old: u32) -> u32 {
    match *m {
        Mode::Octal(p) => (old & !0o777) | p,
        Mode::Sym { op: '+', bits, .. } => old | bits,
        Mode::Sym { op: '-', bits, .. } => old & !bits,
        Mode::Sym { who, bits, .. } => (old & !(who | SPECIAL)) | bits,
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let usage =
        || Error::from("usage: chmod [8r]777 file ... or chmod [augo][+-=][rwxalt] file ...");
    if args.len() < 3 {
        return Err(usage());
    }
    // As Inferno's chmod: a bad mode is reported, then the usage, and a
    // file it cannot change is reported with no status.
    let Some(m) = parse(&args[1]) else {
        eprintln!("chmod: bad mode '{}'", args[1]);
        return Err(usage());
    };
    for f in &args[2..] {
        let old = match purgatorio::stat(f) {
            Ok(d) => d.mode,
            Err(e) => {
                eprintln!("chmod: cannot stat {f}: {e}");
                continue;
            }
        };
        let mut d = Dir::null();
        d.mode = (apply(&m, old) & !DMDIR) | (old & DMDIR);
        if let Err(e) = purgatorio::wstat(f, &d) {
            eprintln!("chmod: cannot wstat {f}: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        let m = |s: &str, old: u32| apply(&parse(s).unwrap(), old);
        assert_eq!(m("755", 0o644), 0o755);
        assert_eq!(m("8r600", DMDIR | 0o777), DMDIR | 0o600);
        assert_eq!(m("+x", 0o644), 0o755);
        assert_eq!(m("u+x", 0o644), 0o744);
        assert_eq!(m("go-r", 0o644), 0o600);
        assert_eq!(m("o=w", 0o777), 0o772);
        assert_eq!(m("+a", 0o644), DMAPPEND | 0o644);
        assert!(parse("9").is_none());
        assert!(parse("u?x").is_none());
    }
}
