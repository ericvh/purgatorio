//! File system prototypes, proto(6): an indented list of names, each
//! optionally followed by `perm uid gid source`, that selects files from an
//! existing tree. A first name of `+` in a directory stands for everything
//! below it, `*` for its entries but not their contents, and `%` for its
//! files only. A name `$user` is the current user.
//!
//! [`read`] walks a prototype over a tree and reports each selected file.

use purgatorio::{DMAPPEND, DMDIR, DMEXCL, Dir};

/// One selected file.
#[derive(Debug, Clone)]
pub struct File {
    /// Its name in the tree being made, starting with `/`.
    pub new: String,
    /// The file it comes from in the current name space.
    pub old: String,
    /// The stat of `old`, with the prototype's permissions applied.
    pub dir: Dir,
    /// The owner and group the prototype asks for, if any.
    pub uid: Option<String>,
    pub gid: Option<String>,
}

/// How serious a problem is: a warning goes on, an error stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Warn,
    Error,
}

/// What the walk needs from the name space; tests supply a fake.
pub trait Fs {
    fn stat(&self, path: &str) -> Result<Dir, String>;
    /// The entries of a directory, sorted by name.
    fn list(&self, path: &str) -> Result<Vec<Dir>, String>;
    fn user(&self) -> String;
}

/// The name space of this prog.
pub struct Namespace;

impl Fs for Namespace {
    fn stat(&self, path: &str) -> Result<Dir, String> {
        purgatorio::stat(path).map_err(|e| e.0)
    }

    fn list(&self, path: &str) -> Result<Vec<Dir>, String> {
        let fd = purgatorio::open(path, purgatorio::abi::mode::OREAD).map_err(|e| e.0)?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        let mut out = Vec::new();
        loop {
            let batch = purgatorio::dirread(raw).map_err(|e| e.0)?;
            if batch.is_empty() {
                break;
            }
            out.extend(batch);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn user(&self) -> String {
        std::fs::read_to_string("/dev/user").unwrap_or_default()
    }
}

/// Joins a name to a prefix with exactly one `/`.
pub fn mkpath(prefix: &str, elem: &str) -> String {
    match (prefix.ends_with('/'), elem.starts_with('/')) {
        (true, true) => format!("{prefix}{}", &elem[1..]),
        (false, false) => format!("{prefix}/{elem}"),
        _ => format!("{prefix}{elem}"),
    }
}

/// A prototype line.
#[derive(Debug, Clone)]
struct Entry {
    indent: usize,
    line: usize,
    name: String,
    mode: Option<u32>,
    uid: Option<String>,
    gid: Option<String>,
    source: Option<String>,
}

type Report<'a> = dyn FnMut(Level, usize, &str) + 'a;

fn field(s: Option<&str>) -> Option<String> {
    s.filter(|s| *s != "-").map(String::from)
}

/// Parses a permission field: `[d][a][l]` and three or more octal digits.
fn mode(s: &str) -> Option<u32> {
    let mut m = 0;
    let mut rest = s;
    for (c, bit) in [('d', DMDIR), ('a', DMAPPEND), ('l', DMEXCL)] {
        if let Some(r) = rest.strip_prefix(c) {
            m |= bit;
            rest = r;
        }
    }
    if rest.len() < 3 || !rest.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    Some(m | u32::from_str_radix(rest, 8).ok()?)
}

fn parse(text: &str, fs: &dyn Fs, report: &mut Report<'_>) -> Result<Vec<Entry>, ()> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let lineno = n + 1;
        let mut indent = 0;
        let mut body = line;
        while let Some(c) = body.chars().next() {
            match c {
                ' ' => indent += 1,
                '\t' => indent += 8,
                _ => break,
            }
            body = &body[1..];
        }
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let mut words = body.split_whitespace();
        let mut name = String::from(words.next().unwrap_or_default());
        if let Some(var) = name.strip_prefix('$') {
            if var != "user" {
                report(
                    Level::Error,
                    lineno,
                    &format!("can't read environment variable {var}"),
                );
                return Err(());
            }
            name = fs.user();
        }
        let perm = words.next();
        let mode = match perm {
            None | Some("-") => None,
            Some(p) => {
                let m = mode(p);
                if m.is_none() {
                    report(Level::Warn, lineno, &format!("bad mode specification {p}"));
                }
                m
            }
        };
        out.push(Entry {
            indent,
            line: lineno,
            name,
            mode,
            uid: field(words.next()),
            gid: field(words.next()),
            source: field(words.next()),
        });
    }
    Ok(out)
}

struct Walk<'a, 'b> {
    fs: &'a dyn Fs,
    root: String,
    report: &'a mut Report<'b>,
    out: &'a mut dyn FnMut(&File),
}

impl Walk<'_, '_> {
    /// The source of `new`: given in the prototype, under a renamed
    /// parent, or under the root.
    fn oldname(&self, new: &str, old: Option<&str>) -> String {
        match old {
            Some(o) if o.starts_with('/') => mkpath(&self.root, o),
            Some(o) => String::from(o),
            None => mkpath(&self.root, new),
        }
    }

    fn emit(&mut self, e: Option<&Entry>, new: String, old: String, mut dir: Dir) -> bool {
        if let Some(m) = e.and_then(|e| e.mode) {
            if (dir.mode & DMDIR) != (m & DMDIR) {
                let line = e.map_or(0, |e| e.line);
                (self.report)(Level::Warn, line, &format!("inconsistent mode for {new}"));
            } else {
                dir.mode = m;
            }
        }
        let isdir = dir.mode & DMDIR != 0;
        (self.out)(&File {
            new,
            old,
            dir,
            uid: e.and_then(|e| e.uid.clone()),
            gid: e.and_then(|e| e.gid.clone()),
        });
        isdir
    }

    /// Everything in `old` (as `new`) for `+`, `*` or `%`.
    fn tree(
        &mut self,
        new: &str,
        old: &str,
        rel: Option<&str>,
        rec: bool,
        files: bool,
        line: usize,
    ) {
        let ents = match self.fs.list(old) {
            Ok(v) => v,
            Err(e) => {
                (self.report)(Level::Warn, line, &format!("can't open {old}: {e}"));
                return;
            }
        };
        for d in ents {
            if files && d.mode & DMDIR != 0 {
                continue;
            }
            let cnew = mkpath(new, &d.name);
            let crel = rel.map(|r| mkpath(r, &d.name));
            let cold = self.oldname(&cnew, crel.as_deref());
            if self.emit(None, cnew.clone(), cold.clone(), d) && rec {
                self.tree(&cnew, &cold, crel.as_deref(), rec, files, line);
            }
        }
    }

    /// The entries `ents` (one level, with their children) under `new`.
    fn level(&mut self, ents: &[Entry], new: &str, old: &str, rel: Option<&str>) {
        let mut i = 0;
        while i < ents.len() {
            let e = &ents[i];
            let end = (i + 1..ents.len())
                .find(|&j| ents[j].indent <= e.indent)
                .unwrap_or(ents.len());
            let children = &ents[i + 1..end];
            if i == 0 && matches!(e.name.as_str(), "+" | "*" | "%") {
                self.tree(new, old, rel, e.name == "+", e.name == "%", e.line);
            } else {
                let cnew = mkpath(new, &e.name);
                let crel = e.source.clone().or_else(|| rel.map(|r| mkpath(r, &e.name)));
                let cold = self.oldname(&cnew, crel.as_deref());
                match self.fs.stat(&cold) {
                    Ok(d) => {
                        if self.emit(Some(e), cnew.clone(), cold.clone(), d) && !children.is_empty()
                        {
                            self.level(children, &cnew, &cold, crel.as_deref());
                        }
                    }
                    Err(err) => (self.report)(
                        Level::Warn,
                        e.line,
                        &format!("can't stat file {cold}: {err}"),
                    ),
                }
            }
            i = end;
        }
    }
}

/// Walks the prototype `text` over the tree at `root`, calling `out` for
/// each selected file in order and `report` for problems. Returns false
/// after an error.
pub fn read(
    text: &str,
    root: &str,
    fs: &dyn Fs,
    report: &mut Report<'_>,
    out: &mut dyn FnMut(&File),
) -> bool {
    let Ok(ents) = parse(text, fs, report) else {
        return false;
    };
    let mut w = Walk {
        fs,
        root: String::from(root),
        report,
        out,
    };
    w.level(&ents, "/", root, None);
    true
}

/// Quotes a name as Inferno's `%q` does, when it is empty or holds white
/// space or a quote.
pub fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '\'') {
        return String::from(s);
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// The CRC-32 (IEEE) that Inferno's crc module computes.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A tree of names; a trailing `/` marks a directory.
    struct Fake(BTreeMap<String, bool>);

    impl Fake {
        fn new(names: &[&str]) -> Fake {
            let mut m = BTreeMap::new();
            m.insert(String::from("/r"), true);
            for n in names {
                let dir = n.ends_with('/');
                m.insert(format!("/r/{}", n.trim_end_matches('/')), dir);
            }
            Fake(m)
        }

        fn dir(name: &str, isdir: bool) -> Dir {
            let mut d = Dir::null();
            d.name = String::from(name.rsplit('/').next().unwrap_or(name));
            d.mode = if isdir { DMDIR | 0o775 } else { 0o664 };
            d
        }
    }

    impl Fs for Fake {
        fn stat(&self, path: &str) -> Result<Dir, String> {
            self.0
                .get(path)
                .map(|&d| Fake::dir(path, d))
                .ok_or_else(|| String::from("file does not exist"))
        }

        fn list(&self, path: &str) -> Result<Vec<Dir>, String> {
            let pre = format!("{path}/");
            Ok(self
                .0
                .iter()
                .filter(|(k, _)| k.starts_with(&pre) && !k[pre.len()..].contains('/'))
                .map(|(k, &d)| Fake::dir(k, d))
                .collect())
        }

        fn user(&self) -> String {
            String::from("bob")
        }
    }

    fn walk(fs: &Fake, text: &str) -> (Vec<String>, Vec<String>) {
        let mut files = Vec::new();
        let mut errs = Vec::new();
        read(
            text,
            "/r",
            fs,
            &mut |l, n, s| errs.push(format!("{l:?} {n}: {s}")),
            &mut |f| files.push(format!("{} {} {:o}", f.new, f.old, f.dir.mode & 0o777)),
        );
        (files, errs)
    }

    #[test]
    fn selections() {
        let fs = Fake::new(&[
            "a/",
            "a/x",
            "a/sub/",
            "a/sub/y",
            "b",
            "usr/",
            "usr/bob/",
            "usr/bob/f",
        ]);
        let (files, errs) = walk(&fs, "a\n\t*\nb 600\nusr\n\t$user\n\t\t+\nnope\n");
        assert_eq!(
            files,
            [
                "/a /r/a 775",
                "/a/sub /r/a/sub 775",
                "/a/x /r/a/x 664",
                "/b /r/b 600",
                "/usr /r/usr 775",
                "/usr/bob /r/usr/bob 775",
                "/usr/bob/f /r/usr/bob/f 664",
            ]
        );
        assert_eq!(
            errs,
            ["Warn 7: can't stat file /r/nope: file does not exist"]
        );
        let (files, _) = walk(&fs, "a\n\t%\n\tsub\n\t\t+\n");
        assert_eq!(
            files,
            [
                "/a /r/a 775",
                "/a/x /r/a/x 664",
                "/a/sub /r/a/sub 775",
                "/a/sub/y /r/a/sub/y 664"
            ]
        );
        let (files, _) = walk(&fs, "+\n");
        assert_eq!(files.len(), 8);
    }

    #[test]
    fn sources_and_modes() {
        let fs = Fake::new(&["a/", "a/x", "b"]);
        let (files, errs) = walk(&fs, "renamed - - - /a\n\tx\nc - - - /b\nb d775\nb 7z7\n");
        assert_eq!(
            files,
            [
                "/renamed /r/a 775",
                "/renamed/x /r/a/x 664",
                "/c /r/b 664",
                "/b /r/b 664",
                "/b /r/b 664",
            ]
        );
        assert_eq!(
            errs,
            [
                "Warn 5: bad mode specification 7z7",
                "Warn 4: inconsistent mode for /b"
            ]
        );
        assert_eq!(mkpath("/", "/a"), "/a");
        assert_eq!(mkpath("/r", "a"), "/r/a");
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
