//! ar key [posname] afile [file ...]: maintain an archive of files in
//! the portable UNIX format, as Inferno's ar(1). The archive is the magic
//! line `!<arch>` and, for each member, a 60-byte header (name, date,
//! uid, gid, octal mode and size, blank-padded) and its data, padded to
//! an even length.
//!
//! The key is one of `d` (delete), `r` (replace or add), `q` (append
//! without looking), `t` (list), `p` (print), `m` (move) or `x`
//! (extract), with any of `v` (verbose), `u` (replace only newer files),
//! `a` or `b`/`i` (place new or moved files after or before `posname`),
//! `c` (create without a warning), `l` (ignored) and `o` (keep the
//! modification times of extracted files). A member is named by its
//! file's last path element, at most 16 bytes. Names given that the
//! archive does not have are reported (`ar: name not found`) and give
//! an error status.
//!
//! The archive is rewritten from memory rather than through temporary
//! files.

use std::fs::File;
use std::io::{Read, Write};

use purgatorio::{Context, Dir, Error, daytime};

const ARMAG: &[u8] = b"!<arch>\n";
const SAR_HDR: usize = 60;
const NAME_LEN: usize = 16;
const MAN: &str = "mrxtdpq";
const OPT: &str = "uvnbailo";

#[derive(Clone, Copy, PartialEq)]
enum Cmd {
    D,
    R,
    Q,
    T,
    P,
    M,
    X,
}

#[derive(Clone)]
struct Member {
    name: String,
    date: u32,
    uid: u32,
    gid: u32,
    mode: u32,
    data: Vec<u8>,
}

struct FileArg {
    name: String,
    trimmed: String,
    found: bool,
}

struct Ar {
    cmd: Cmd,
    aflag: bool,
    bflag: bool,
    cflag: bool,
    oflag: bool,
    uflag: bool,
    vflag: bool,
    pivot: Option<String>,
    /// Where `bamatch` is: looking for the pivot (0), just past it with
    /// `a` (1), or past it (2).
    state: u8,
    files: Vec<FileArg>,
    out: Vec<u8>,
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 3 {
        usage();
    }
    let mut cmd = None;
    let mut ar = Ar {
        cmd: Cmd::R,
        aflag: false,
        bflag: false,
        cflag: false,
        oflag: false,
        uflag: false,
        vflag: false,
        pivot: None,
        state: 0,
        files: Vec::new(),
        out: Vec::new(),
    };
    let mut setcom = |c: Cmd| {
        if cmd.is_some() {
            eprintln!("ar: only one of [{MAN}] allowed");
            usage();
        }
        cmd = Some(c);
    };
    for c in args[1].chars() {
        match c {
            'a' => ar.aflag = true,
            'b' | 'i' => ar.bflag = true,
            'c' => ar.cflag = true,
            'l' => {}
            'o' => ar.oflag = true,
            'u' => ar.uflag = true,
            'v' => ar.vflag = true,
            'd' => setcom(Cmd::D),
            'm' => setcom(Cmd::M),
            'p' => setcom(Cmd::P),
            'q' => setcom(Cmd::Q),
            'r' => setcom(Cmd::R),
            't' => setcom(Cmd::T),
            'x' => setcom(Cmd::X),
            _ => {
                eprintln!("ar: bad option `{c}'");
                usage();
            }
        }
    }
    let mut rest = &args[2..];
    if ar.aflag && ar.bflag {
        eprintln!("ar: only one of 'a' and 'b' can be specified");
        usage();
    }
    if ar.aflag || ar.bflag {
        ar.pivot = Some(trim(&rest[0]));
        rest = &rest[1..];
        if rest.len() < 2 {
            usage();
        }
    }
    ar.cmd = match cmd {
        Some(c) => c,
        None if ar.uflag => Cmd::R,
        None => {
            eprintln!("ar: one of [{MAN}] must be specified");
            usage();
        }
    };
    let arname = rest[0].clone();
    ar.files = rest[1..]
        .iter()
        .map(|f| FileArg {
            name: f.clone(),
            trimmed: trim(f),
            found: false,
        })
        .collect();
    match ar.cmd {
        Cmd::R => ar.rcmd(&arname),
        Cmd::D => ar.dcmd(&arname),
        Cmd::X => ar.xcmd(&arname),
        Cmd::P => ar.pcmd(&arname),
        Cmd::M => ar.mcmd(&arname),
        Cmd::T => ar.tcmd(&arname),
        Cmd::Q => ar.qcmd(&arname),
    }
    let mut allfound = true;
    for f in &ar.files {
        if !f.found {
            eprintln!("ar: {} not found", f.name);
            allfound = false;
        }
    }
    ar.flush();
    if !allfound {
        purgatorio::exits("fail: file not found");
    }
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: ar [{OPT}][{MAN}] archive files ...");
    purgatorio::exits("fail:usage");
}

fn fail(msg: &str, status: &str) -> ! {
    eprintln!("ar: {msg}");
    purgatorio::exits(status);
}

/// The last element of a path, without trailing slashes.
fn trim(s: &str) -> String {
    let s = s.trim_end_matches('/');
    s.rsplit('/').next().unwrap_or(s).to_string()
}

fn errstr(e: std::io::Error) -> String {
    Error::from(e).0
}

/// The members of an archive file, or `None` if it cannot be opened and
/// `errok` says that is fine.
fn read_archive(arname: &str, errok: bool) -> Option<Vec<Member>> {
    let data = match std::fs::read(arname) {
        Ok(d) => d,
        Err(e) => {
            if errok {
                return None;
            }
            fail(
                &format!("cannot open {arname}: {}", errstr(e)),
                "fail:error",
            );
        }
    };
    if !data.starts_with(ARMAG) {
        fail(&format!("{arname} not in archive format"), "fail:error");
    }
    Some(parse(&data))
}

fn parse(data: &[u8]) -> Vec<Member> {
    let mut out = Vec::new();
    let mut o = ARMAG.len();
    while o < data.len() {
        let Some(h) = data.get(o..o + SAR_HDR) else {
            fail("read error: unexpected end-of-file", "fail:read error");
        };
        if h[58] != b'`' || h[59] != b'\n' {
            fail(&format!("phase error at offset {o}"), "fail:phase error");
        }
        let name = String::from_utf8_lossy(&h[..NAME_LEN])
            .trim_end_matches([' ', '/'])
            .to_string();
        let size = intof(&h[48..58], 10) as usize;
        let start = o + SAR_HDR;
        let Some(body) = data.get(start..start + size) else {
            fail("read error: unexpected end-of-file", "fail:read error");
        };
        out.push(Member {
            name,
            date: intof(&h[16..28], 10),
            uid: intof(&h[28..34], 10),
            gid: intof(&h[34..40], 10),
            mode: intof(&h[40..48], 8),
            data: body.to_vec(),
        });
        o = start + size + (size & 1);
    }
    out
}

fn intof(a: &[u8], base: u32) -> u32 {
    let s = String::from_utf8_lossy(a);
    let t = s.trim();
    if t.is_empty() {
        return 0;
    }
    match u32::from_str_radix(t, base) {
        Ok(n) => n,
        Err(_) => fail(
            &format!(
                "invalid integer in archive member's header: {}",
                quote(s.trim_end())
            ),
            "fail:error",
        ),
    }
}

fn quote(s: &str) -> String {
    if !s.is_empty() && !s.contains([' ', '\t', '\'', '\n']) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "''"))
}

fn header(m: &Member) -> Vec<u8> {
    let mut b = vec![b' '; SAR_HDR];
    let name = m.name.as_bytes();
    let n = name.len().min(NAME_LEN);
    b[..n].copy_from_slice(&name[..n]);
    let mut put = |at: usize, s: String| b[at..at + s.len()].copy_from_slice(s.as_bytes());
    put(16, format!("{:<12}", m.date));
    put(28, format!("{:<6}", 0));
    put(34, format!("{:<6}", 0));
    put(40, format!("{:<8o}", m.mode));
    put(48, format!("{:<10}", m.data.len()));
    b[58] = b'`';
    b[59] = b'\n';
    b
}

fn pack(m: &Member, out: &mut Vec<u8>) {
    out.extend_from_slice(&header(m));
    out.extend_from_slice(&m.data);
    if m.data.len() & 1 == 1 {
        out.push(b'\n');
    }
}

/// A new member from the file `name`.
fn member(name: &str, f: &mut File) -> Member {
    let d = match purgatorio::fstat(std::os::fd::AsRawFd::as_raw_fd(f)) {
        Ok(d) => d,
        Err(e) => fail(&format!("cannot stat {name}: {e}"), "fail:no stat"),
    };
    let mut data = Vec::new();
    if let Err(e) = f.read_to_end(&mut data) {
        fail(&format!("read error: {}", errstr(e)), "fail:read error");
    }
    Member {
        name: trim(name),
        date: d.mtime,
        uid: 0,
        gid: 0,
        mode: d.mode & 0o777,
        data,
    }
}

fn modes(mode: u32) -> String {
    const T: [&str; 8] = ["---", "--x", "-w-", "-wx", "r--", "r-x", "rw-", "rwx"];
    format!(
        "{}{}{}",
        T[(mode >> 6) as usize & 7],
        T[(mode >> 3) as usize & 7],
        T[mode as usize & 7]
    )
}

fn longtext(m: &Member) -> String {
    let t = daytime::text(&daytime::gmt(i64::from(m.date)));
    format!(
        "{} {:3}/{:1} {:7} {:<12.12} {:<4.4} ",
        modes(m.mode),
        m.uid,
        m.gid,
        m.data.len(),
        t.get(4..).unwrap_or(""),
        t.get(24..).unwrap_or("")
    )
}

fn install(arname: &str, parts: &[Vec<Member>], creating: bool) {
    if creating {
        eprintln!("ar: creating {arname}");
    }
    let mut out = ARMAG.to_vec();
    for m in parts.iter().flatten() {
        pack(m, &mut out);
    }
    let mut f = match File::create(arname) {
        Ok(f) => f,
        Err(e) => fail(
            &format!("cannot create {arname}: {}", errstr(e)),
            "fail:create",
        ),
    };
    if let Err(e) = f.write_all(&out) {
        fail(&format!("write error: {}", errstr(e)), "fail:write error");
    }
}

impl Ar {
    fn mesg(&mut self, c: char, file: &str) {
        if self.vflag {
            self.out
                .extend_from_slice(format!("{c} - {file}\n").as_bytes());
        }
    }

    fn flush(&mut self) {
        let _ = std::io::stdout().write_all(&self.out);
        self.out.clear();
    }

    /// The index of the unfound file argument named `name`, marking it
    /// found; with no file arguments every member matches (`Some(None)`).
    fn matches(&mut self, name: &str) -> Option<Option<usize>> {
        if self.files.is_empty() {
            return Some(None);
        }
        let i = self
            .files
            .iter()
            .position(|f| !f.found && f.trimmed == name)?;
        self.files[i].found = true;
        Some(Some(i))
    }

    fn display(&self, m: Option<usize>, name: &str) -> String {
        m.map_or_else(|| name.to_string(), |i| self.files[i].name.clone())
    }

    /// Whether `file` is where the new or moved members go.
    fn bamatch(&mut self, file: &str) -> bool {
        match self.state {
            0 => {
                if self.aflag && self.pivot.as_deref() == Some(file) {
                    self.state = 1;
                } else if self.bflag && self.pivot.as_deref() == Some(file) {
                    self.state = 2;
                    return true;
                }
            }
            1 => {
                self.state = 2;
                return true;
            }
            _ => {}
        }
        false
    }

    fn rcmd(&mut self, arname: &str) {
        let old = read_archive(arname, true);
        let mut parts: Vec<Vec<Member>> = vec![Vec::new(), Vec::new()];
        let mut ap = 0;
        for mut mem in old.clone().unwrap_or_default() {
            if self.bamatch(&mem.name) {
                ap = 1;
            }
            let Some(m) = self.matches(&mem.name) else {
                parts[ap].push(mem);
                continue;
            };
            let fname = self.display(m, &mem.name);
            let mut f = match File::open(&fname) {
                Ok(f) => f,
                Err(e) => {
                    if !self.files.is_empty() {
                        eprintln!("ar: cannot open {fname}: {}", errstr(e));
                    }
                    parts[ap].push(mem);
                    continue;
                }
            };
            if self.uflag {
                match purgatorio::fstat(std::os::fd::AsRawFd::as_raw_fd(&f)) {
                    Ok(d) if d.mtime > mem.date => {}
                    r => {
                        if let Err(e) = r {
                            eprintln!("ar: cannot stat {fname}: {e}");
                        }
                        parts[ap].push(mem);
                        continue;
                    }
                }
            }
            self.mesg('r', &fname);
            mem = member(&fname, &mut f);
            parts[ap].push(mem);
        }
        for i in 0..self.files.len() {
            if self.files[i].found {
                continue;
            }
            self.files[i].found = true;
            let name = self.files[i].name.clone();
            match File::open(&name) {
                Ok(mut f) => {
                    self.mesg('a', &name);
                    let m = member(&name, &mut f);
                    parts[0].push(m);
                }
                Err(e) => eprintln!("ar: cannot open {name}: {}", errstr(e)),
            }
        }
        self.flush();
        install(arname, &parts, old.is_none() && !self.cflag);
    }

    fn dcmd(&mut self, arname: &str) {
        if self.files.is_empty() {
            return;
        }
        let mut keep = Vec::new();
        let mut changed = false;
        for mem in read_archive(arname, false).unwrap_or_default() {
            if self.matches(&mem.name).is_some() {
                self.mesg('d', &mem.name);
                changed = true;
            } else {
                keep.push(mem);
            }
        }
        self.flush();
        if changed {
            install(arname, &[keep], false);
        }
    }

    fn xcmd(&mut self, arname: &str) {
        let n = self.files.len();
        let mut i = 0;
        for mem in read_archive(arname, false).unwrap_or_default() {
            let Some(m) = self.matches(&mem.name) else {
                continue;
            };
            let fname = self.display(m, &mem.name);
            match purgatorio::create(&fname, purgatorio::abi::mode::OWRITE, mem.mode & 0o777) {
                Err(e) => eprintln!("ar: cannot create {fname}: {e}"),
                Ok(fd) => {
                    self.mesg('x', &fname);
                    let mut f = File::from(fd);
                    if let Err(e) = f.write_all(&mem.data) {
                        fail(&format!("write error: {}", errstr(e)), "fail:write error");
                    }
                    if self.oflag {
                        let mut d = Dir::null();
                        d.atime = mem.date;
                        d.mtime = mem.date;
                        if let Err(e) = purgatorio::fwstat(std::os::fd::AsRawFd::as_raw_fd(&f), &d)
                        {
                            eprint!("ar: can't set times on {fname}: {e}");
                        }
                    }
                }
            }
            i += 1;
            if n > 0 && i >= n {
                break;
            }
        }
    }

    fn pcmd(&mut self, arname: &str) {
        let n = self.files.len();
        let mut i = 0;
        for mem in read_archive(arname, false).unwrap_or_default() {
            let Some(m) = self.matches(&mem.name) else {
                continue;
            };
            if self.vflag {
                let name = self.display(m, &mem.name);
                self.out
                    .extend_from_slice(format!("\n<{name}>\n\n").as_bytes());
            }
            self.out.extend_from_slice(&mem.data);
            i += 1;
            if n > 0 && i >= n {
                break;
            }
        }
    }

    fn mcmd(&mut self, arname: &str) {
        if self.files.is_empty() {
            return;
        }
        let mut parts: Vec<Vec<Member>> = vec![Vec::new(), Vec::new(), Vec::new()];
        let mut ap = 0;
        let mut found_pivot = false;
        for mem in read_archive(arname, false).unwrap_or_default() {
            if self.bamatch(&mem.name) {
                ap = 2;
                found_pivot = true;
            }
            if let Some(m) = self.matches(&mem.name) {
                let name = self.display(m, &mem.name);
                self.mesg('m', &name);
                parts[1].push(mem);
            } else {
                parts[ap].push(mem);
            }
        }
        if let Some(p) = &self.pivot
            && !found_pivot
        {
            eprintln!("ar: {p} not found - files moved to end");
        }
        self.flush();
        install(arname, &parts, false);
    }

    fn tcmd(&mut self, arname: &str) {
        for mem in read_archive(arname, false).unwrap_or_default() {
            let Some(m) = self.matches(&mem.name) else {
                continue;
            };
            let name = m.map_or_else(|| mem.name.clone(), |i| self.files[i].trimmed.clone());
            let long = if self.vflag {
                longtext(&mem) + " "
            } else {
                String::new()
            };
            self.out
                .extend_from_slice(format!("{long}{name}\n").as_bytes());
        }
    }

    fn qcmd(&mut self, arname: &str) {
        if self.aflag || self.bflag {
            eprintln!("ar: abi not allowed with q");
            purgatorio::exits("fail:usage");
        }
        let exists = match std::fs::read(arname) {
            Ok(d) => {
                if !d.starts_with(ARMAG) {
                    fail(&format!("{arname} not in archive format"), "fail:error");
                }
                true
            }
            Err(_) => false,
        };
        let mut out = Vec::new();
        if !exists {
            if !self.cflag {
                eprintln!("ar: creating {arname}");
            }
            out.extend_from_slice(ARMAG);
        }
        for i in 0..self.files.len() {
            self.files[i].found = true;
            let name = self.files[i].name.clone();
            match File::open(&name) {
                Ok(mut f) => {
                    self.mesg('q', &name);
                    let m = member(&name, &mut f);
                    pack(&m, &mut out);
                }
                Err(e) => eprintln!("ar: cannot open {name}: {}", errstr(e)),
            }
        }
        let f = if exists {
            std::fs::OpenOptions::new().append(true).open(arname)
        } else {
            File::create(arname)
        };
        match f {
            Ok(mut f) => {
                if let Err(e) = f.write_all(&out) {
                    fail(&format!("write error: {}", errstr(e)), "fail:write error");
                }
            }
            Err(e) => fail(
                &format!("cannot create {arname}: {}", errstr(e)),
                "fail:create",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = Member {
            name: String::from("hello.txt"),
            date: 1000,
            uid: 0,
            gid: 0,
            mode: 0o644,
            data: b"odd".to_vec(),
        };
        let mut out = ARMAG.to_vec();
        pack(&m, &mut out);
        pack(&m, &mut out);
        assert_eq!(out.len(), 8 + 2 * (60 + 4));
        assert_eq!(
            &out[8..68],
            b"hello.txt       1000        0     0     644     3         `\n"
        );
        let back = parse(&out);
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].data, b"odd");
        assert_eq!(back[1].mode, 0o644);
        assert_eq!(trim("a/b/c.o/"), "c.o");
        assert_eq!(modes(0o754), "rwxr-xr--");
    }
}
