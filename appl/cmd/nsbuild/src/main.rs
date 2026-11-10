//! nsbuild [file]: change the namespace by the commands in `file` (default
//! `namespace` in the current directory), in the format of namespace(6).
//!
//! Each line is a command, its words split with sh quoting. Blank lines
//! and lines whose first word starts with `#` are skipped. `$name` in a
//! word is the value of environment variable `name`, which ends at `/`,
//! `.`, `$` or the end of the word; a line naming a variable that is not
//! set is skipped. The commands:
//!
//! ```text
//! bind [-abcri] from to
//! mount [-abcriA9] [-k keyfile] [-C alg] addr to [spec]
//! import [-abcri9] addr [rdir] to
//! unmount [-i] [from] to
//! cd dir
//! . file
//! fork
//! new
//! nodev
//! clear
//! ```
//!
//! `-a`, `-b` and `-r` place `from` after, before or instead of what is
//! at `to`, `-c` allows creation, and `-i` ignores the command's
//! failure. `mount` dials `addr` (`[net!]host[!service]`, service `styx` by
//! default) and authenticates unless given `-A`; `import` dials service
//! 17007 and mounts what it serves, passing `rdir` as the attach name.
//! `-k`, `-C` and `-9` are accepted for compatibility: authentication here
//! always uses the user's key. The namespace nsbuild changes is the one it
//! shares with the shell that ran it, so `cd`, `fork`, `new`, `nodev` and
//! `clear` change only nsbuild's own and have no lasting effect.
//! Building stops at the first command that fails.

use std::os::fd::AsRawFd;

use purgatorio::abi::{mount, pctl};
use purgatorio::{Context, Error};

/// Inferno's `%q`: the word as is, or in single quotes when it is empty or
/// holds white space or a quote.
fn q(s: &str) -> String {
    if !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// `word` with `$name` expanded, or `None` when a variable is not set.
fn expand(word: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = word;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after
            .find(|c: char| c == '/' || c == '.' || c == '$' || c.is_whitespace())
            .unwrap_or(after.len());
        out.push_str(&env(&after[..end])?);
        rest = &after[end..];
    }
    out.push_str(rest);
    Some(out)
}

/// The flags of a command: the bind and mount flags, whether to ignore
/// failure, and the arguments after them.
struct Opts {
    flags: u32,
    ignore: bool,
    noauth: bool,
    args: Vec<String>,
}

fn opts(verb: &str, words: &[String], extra: &str) -> Result<Opts, String> {
    let mut o = Opts {
        flags: mount::MREPL,
        ignore: false,
        noauth: false,
        args: Vec::new(),
    };
    let mut i = 0;
    while let Some(w) = words.get(i) {
        if !w.starts_with('-') || w == "-" {
            break;
        }
        i += 1;
        if w == "--" {
            break;
        }
        for c in w[1..].chars() {
            match c {
                'a' => o.flags = (o.flags & !mount::MORDER) | mount::MAFTER,
                'b' => o.flags = (o.flags & !mount::MORDER) | mount::MBEFORE,
                'r' => o.flags &= !mount::MORDER,
                'c' => o.flags |= mount::MCREATE,
                'i' => o.ignore = true,
                'A' if extra.contains('A') => o.noauth = true,
                '9' if extra.contains('9') => {}
                'k' | 'C' if extra.contains(c) => {
                    if words.get(i).is_none() {
                        return Err(format!("{verb}: missing arg to -{c} option"));
                    }
                    i += 1;
                }
                c => return Err(format!("{verb}: bad option -{c}")),
            }
        }
    }
    o.args = words[i..].to_vec();
    Ok(o)
}

/// `[net!]host[!service]` with the defaults filled in.
fn addr(a: &str, service: &str) -> String {
    match a.split('!').count() {
        1 => format!("tcp!{a}!{service}"),
        2 => format!("{a}!{service}"),
        _ => String::from(a),
    }
}

fn dial_mount(a: &str, service: &str, old: &str, flags: u32, spec: &str) -> Result<(), String> {
    let conn = purgatorio::dial::dial(&addr(a, service)).map_err(|e| format!("dial: {a}: {e}"))?;
    let fd = conn
        .data
        .as_ref()
        .ok_or_else(|| format!("dial: {a}: no connection"))?
        .as_raw_fd();
    let r = if flags & mount::MNOAUTH != 0 {
        purgatorio::mount(fd, -1, old, flags, spec)
    } else {
        purgatorio::auth::amount(fd, old, flags, spec)
    };
    r.map_err(|e| format!("mount {} {}: {e}", q(a), q(old)))
}

struct Builder {
    /// Files being read, to stop an include loop.
    depth: usize,
}

impl Builder {
    fn file(&mut self, path: &str) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "can't open {} for read {}",
                q(path),
                purgatorio::Error::from(e)
            )
        })?;
        self.depth += 1;
        if self.depth > 32 {
            return Err(format!("{}: too many nested files", q(path)));
        }
        let env = |n: &str| purgatorio::env::get(n);
        for line in text.lines() {
            let words = purgatorio::unquote_fields(line);
            if words.first().is_none_or(|w| w.starts_with('#')) {
                continue;
            }
            let Some(words) = words
                .iter()
                .map(|w| expand(w, &env))
                .collect::<Option<Vec<String>>>()
            else {
                continue;
            };
            self.line(&words)?;
        }
        self.depth -= 1;
        Ok(())
    }

    fn line(&mut self, w: &[String]) -> Result<(), String> {
        let rest = &w[1..];
        match w[0].as_str() {
            "bind" => {
                let o = opts("bind", rest, "")?;
                let [new, old] = o.args.as_slice() else {
                    if o.args.len() < 2 {
                        return Err(String::from("bind: too few args"));
                    }
                    return Err(String::from("bind: too many args"));
                };
                match purgatorio::bind(new, old, o.flags) {
                    Err(e) if !o.ignore => Err(format!("bind {} {}: {e}", q(new), q(old))),
                    _ => Ok(()),
                }
            }
            "mount" => {
                let o = opts("mount", rest, "Ak9C")?;
                let (a, old, spec) = match o.args.as_slice() {
                    [a, old] => (a, old, ""),
                    [a, old, spec] => (a, old, spec.as_str()),
                    _ => return Err(String::from("mount: too few args")),
                };
                let flags = o.flags | if o.noauth { mount::MNOAUTH } else { 0 };
                match dial_mount(a, "6666", old, flags, spec) {
                    Err(e) if !o.ignore => Err(e),
                    _ => Ok(()),
                }
            }
            "import" => {
                let o = opts("import", rest, "9")?;
                let (a, rdir, old) = match o.args.as_slice() {
                    [a, old] => (a, "", old),
                    [a, rdir, old] => (a, rdir.as_str(), old),
                    _ => return Err(String::from("import: too few args")),
                };
                match dial_mount(a, "17007", old, o.flags, rdir) {
                    Err(e) if !o.ignore => Err(e
                        .replacen("mount ", "import ", 1)
                        .replacen("dial: ", "import: ", 1)),
                    _ => Ok(()),
                }
            }
            "unmount" => {
                let o = opts("unmount", rest, "")?;
                let r = match o.args.as_slice() {
                    [old] => purgatorio::unmount(None, old),
                    [new, old] => purgatorio::unmount(Some(new), old),
                    _ => return Err(String::from("unmount: takes 1 or 2 args")),
                };
                match r {
                    Err(e) if !o.ignore => Err(format!("unmount: {e}")),
                    _ => Ok(()),
                }
            }
            "cd" => {
                let [dir] = rest else {
                    return Err(String::from("cd: must have one argument"));
                };
                purgatorio::chdir(dir).map_err(|e| e.0)
            }
            "." => {
                let [file] = rest else {
                    return Err(String::from(".: needs a filename"));
                };
                self.file(file)
            }
            "fork" => purgatorio::pctl(pctl::FORKNS, &[])
                .map(drop)
                .map_err(|e| e.0),
            "new" => purgatorio::pctl(pctl::NEWNS | pctl::FORKENV, &[])
                .map(drop)
                .map_err(|e| e.0),
            "nodev" => purgatorio::pctl(pctl::NODEVS, &[])
                .map(drop)
                .map_err(|e| e.0),
            "clear" => {
                purgatorio::pctl(pctl::FORKNS, &[]).map_err(|e| e.0)?;
                purgatorio::bind("#/", "/", mount::MREPL).map_err(|e| e.0)?;
                purgatorio::chdir("/").map_err(|e| e.0)?;
                purgatorio::pctl(pctl::NEWNS, &[])
                    .map(drop)
                    .map_err(|e| e.0)
            }
            _ => Err(String::from("invalid namespace command")),
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() > 2 {
        eprintln!("Usage: nsbuild [nsfile]");
        purgatorio::exits("usage");
    }
    let file = args.get(1).map_or("namespace", String::as_str);
    Builder { depth: 0 }.file(file).map_err(|e| {
        eprintln!("nsbuild: error building namespace: {e}");
        purgatorio::exits(&format!("fail:{e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables() {
        let env = |n: &str| (n == "user").then(|| String::from("ann"));
        assert_eq!(
            expand("/usr/$user/lib", &env).as_deref(),
            Some("/usr/ann/lib")
        );
        assert_eq!(expand("$user.x", &env).as_deref(), Some("ann.x"));
        assert_eq!(expand("$nope", &env), None);
        assert_eq!(expand("plain", &env).as_deref(), Some("plain"));
    }

    #[test]
    fn options() {
        let w = |s: &[&str]| s.iter().map(|s| String::from(*s)).collect::<Vec<_>>();
        let o = opts("bind", &w(&["-bc", "a", "b"]), "").unwrap();
        assert_eq!(o.flags, mount::MBEFORE | mount::MCREATE);
        assert_eq!(o.args, ["a", "b"]);
        assert!(opts("bind", &w(&["-A", "a", "b"]), "").is_err());
        let o = opts("mount", &w(&["-A", "-k", "key", "tcp!h!1", "/n"]), "Ak9C").unwrap();
        assert!(o.noauth);
        assert_eq!(o.args, ["tcp!h!1", "/n"]);
        assert_eq!(
            opts("mount", &w(&["-k"]), "Ak9C").err().as_deref(),
            Some("mount: missing arg to -k option")
        );
        assert_eq!(addr("h", "6666"), "tcp!h!6666");
        assert_eq!(addr("tcp!h", "6666"), "tcp!h!6666");
        assert_eq!(q("a b"), "'a b'");
    }
}
