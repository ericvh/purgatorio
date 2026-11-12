//! Gates: tests of one entry.

use purg_styx::{DMAPPEND, DMAUTH, DMDIR, DMEXCL};

use crate::tree::Entry;

pub type Gate = Box<dyn Fn(&Entry) -> bool>;

/// One clause of a mode specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clause {
    op: char,
    /// The bits the clause names.
    bits: u32,
    /// For `=`: the permission bits of the classes it names.
    within: u32,
}

/// Parses `[ugoa]*[+-=][rwxdaAl]+`, clauses separated by commas, as
/// chmod(1) writes them: `u+w`, `+d`, `-d`, `go=r`. With no class, `r`,
/// `w` and `x` are for all three. With no operator the clause is `+`.
pub fn parse_mode(spec: &str) -> Result<Vec<Clause>, String> {
    let bad = || format!("bad mode {spec}");
    let mut out = Vec::new();
    for clause in spec.split(',') {
        let mut who = 0u32;
        let mut chars = clause.chars().peekable();
        while let Some(c) = chars.next_if(|c| "ugoa".contains(*c)) {
            who |= match c {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                _ => 0o777,
            };
        }
        // `a` and `d` are also permission letters: `a+r` has a class,
        // `+a` and plain `a` name the append bit.
        let op = match chars.next_if(|c| "+-=".contains(*c)) {
            Some(op) => op,
            None if who == 0 => '+',
            None => return Err(bad()),
        };
        if who == 0 {
            who = 0o777;
        }
        let mut bits = 0u32;
        let mut any = false;
        for c in chars {
            any = true;
            bits |= match c {
                'r' => who & 0o444,
                'w' => who & 0o222,
                'x' => who & 0o111,
                'd' => DMDIR,
                'a' => DMAPPEND,
                'A' => DMAUTH,
                'l' => DMEXCL,
                _ => return Err(bad()),
            };
        }
        if !any {
            return Err(bad());
        }
        out.push(Clause {
            op,
            bits,
            within: who,
        });
    }
    Ok(out)
}

/// Whether `mode` satisfies every clause: `+` wants all of the bits set,
/// `-` none of them, and `=` the named classes' permissions to be exactly
/// the ones given (with any other bits named set).
pub fn mode_ok(clauses: &[Clause], mode: u32) -> bool {
    clauses.iter().all(|c| match c.op {
        '+' => mode & c.bits == c.bits,
        '-' => mode & c.bits == 0,
        _ => {
            let perm = c.bits & 0o777;
            mode & c.within == perm && mode & (c.bits & !0o777) == c.bits & !0o777
        }
    })
}

/// `mode` changed by each clause in turn, as chmod(1) changes it: `+`
/// sets the bits, `-` clears them and `=` makes the named classes'
/// permissions exactly the ones given. Whether it is a directory never
/// changes.
pub fn apply_mode(clauses: &[Clause], mode: u32) -> u32 {
    let mut m = mode;
    for c in clauses {
        m = match c.op {
            '+' => m | c.bits,
            '-' => m & !c.bits,
            _ => (m & !c.within) | c.bits,
        };
    }
    (m & !DMDIR) | (mode & DMDIR)
}

fn clean(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|e| !e.is_empty() && *e != ".")
        .collect()
}

/// Whether `entry` is on the way to `target`, is it, or lies under it.
pub fn path_ok(entry: &str, target: &str) -> bool {
    let (e, t) = (clean(entry), clean(target));
    if entry.starts_with('/') != target.starts_with('/') {
        return false;
    }
    if e.len() <= t.len() {
        t.starts_with(&e)
    } else {
        e.starts_with(&t)
    }
}

/// Whether `entry` lies under `target` (and is not `target` itself).
pub fn under(entry: &str, target: &str) -> bool {
    let (e, t) = (clean(entry), clean(target));
    entry.starts_with('/') == target.starts_with('/') && e.len() > t.len() && e.starts_with(&t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(spec: &str, mode: u32) -> bool {
        mode_ok(&parse_mode(spec).unwrap(), mode)
    }

    #[test]
    fn modes() {
        assert!(ok("+d", DMDIR | 0o755));
        assert!(!ok("+d", 0o644));
        assert!(ok("-d", 0o644));
        assert!(ok("d", DMDIR));
        assert!(ok("u+w", 0o200));
        assert!(!ok("u+w", 0o444));
        assert!(ok("+x", 0o111));
        assert!(!ok("+x", 0o110));
        assert!(ok("go-w", 0o755));
        assert!(!ok("go-w", 0o775));
        assert!(ok("u=rw", 0o640));
        assert!(!ok("u=rw", 0o740));
        assert!(ok("a+r,-d", 0o444));
        assert!(!ok("a+r,-d", DMDIR | 0o444));
        assert!(ok("+al", DMAPPEND | DMEXCL));
        for bad in ["", "u", "u+", "+q", "z+r", "u+r,"] {
            assert!(parse_mode(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn changes() {
        let ch = |spec: &str, mode: u32| apply_mode(&parse_mode(spec).unwrap(), mode);
        assert_eq!(ch("u+w", 0o444), 0o644);
        assert_eq!(ch("-w", 0o666), 0o444);
        assert_eq!(ch("go=r", 0o777), 0o744);
        assert_eq!(ch("+x,o-r", 0o644), 0o751);
        assert_eq!(ch("-d", DMDIR | 0o755), DMDIR | 0o755);
        assert_eq!(ch("+d", 0o644), 0o644);
        assert_eq!(ch("+a", 0o644), DMAPPEND | 0o644);
    }

    #[test]
    fn paths() {
        assert!(path_ok("/a", "/a/b"));
        assert!(path_ok("/a/b", "/a/b"));
        assert!(path_ok("/a/b/c", "/a/b"));
        assert!(!path_ok("/a/c", "/a/b"));
        assert!(path_ok("./a//b", "a/b/c"));
        assert!(!path_ok("a/b", "/a/b"));
        assert!(path_ok("/", "/a"));
        assert!(under("/a/b/c", "/a/b"));
        assert!(!under("/a/b", "/a/b"));
        assert!(!under("/a/c", "/a/b"));
    }
}
