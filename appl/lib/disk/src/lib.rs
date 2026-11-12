//! What disk/prep, disk/fdisk, disk/mbr and disk/format share: the disk a
//! command works on ([`Disk`]: a `#S` disk's `data`, one of its
//! partitions, or an image file) and the partition table editor of
//! prep(8) and fdisk(8) ([`Edit`]).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Seek, SeekFrom, Write};

/// A disk, a partition of one, or an image file, read and written a
/// sector at a time.
pub struct Disk {
    pub path: String,
    file: File,
    /// Bytes per sector.
    pub secsize: u64,
    /// Sectors.
    pub secs: u64,
    /// Where it starts on its disk, in sectors: the partition's start in
    /// the disk's `ctl`, else 0.
    pub offset: u64,
    /// The `ctl` file beside it, if there is one.
    pub ctl: Option<String>,
    /// The partitions the `ctl` file lists, other than `data`.
    pub ctl_parts: Vec<(String, u64, u64)>,
}

impl Disk {
    /// Opens `path`. The sector size is `secsize` if given, else the
    /// `geometry` line of the `ctl` file in the same directory, else 512
    /// (with a warning from `prog`).
    pub fn open(prog: &str, path: &str, secsize: Option<u64>, write: bool) -> Result<Disk, String> {
        let at = |e: std::io::Error| format!("{path}: {}", purgatorio::Error::from(e));
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .open(path)
            .map_err(at)?;
        let length = purgatorio::stat(path)
            .map_err(|e| format!("{path}: {e}"))?
            .length;
        let (dir, base) = match path.rsplit_once('/') {
            Some(("", b)) => ("/", b),
            Some((d, b)) => (d, b),
            None => (".", path),
        };
        let ctl = format!("{}/ctl", dir.trim_end_matches('/'));
        let text = std::fs::read_to_string(&ctl).ok();
        let mut geometry = None;
        let mut offset = 0;
        let mut ctl_parts = Vec::new();
        for line in text.as_deref().unwrap_or("").lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            match f.as_slice() {
                ["geometry", _, bs, ..] => geometry = bs.parse::<u64>().ok(),
                ["part", name, start, end] => {
                    let (Ok(s), Ok(e)) = (start.parse::<u64>(), end.parse::<u64>()) else {
                        continue;
                    };
                    if *name == base {
                        offset = s;
                    }
                    if *name != "data" {
                        ctl_parts.push((String::from(*name), s, e));
                    }
                }
                _ => {}
            }
        }
        let secsize = match (secsize, geometry) {
            (Some(s), _) | (None, Some(s)) => s,
            (None, None) => {
                eprintln!("{prog}: can't read {ctl}; assuming 512-byte sectors");
                512
            }
        };
        if secsize == 0 || !secsize.is_power_of_two() {
            return Err(format!("bad sector size {secsize}"));
        }
        Ok(Disk {
            path: String::from(path),
            file,
            secsize,
            secs: length / secsize,
            offset,
            ctl: text.map(|_| ctl),
            ctl_parts,
        })
    }

    /// Reads `n` sectors from sector `s`.
    pub fn read(&mut self, s: u64, n: u64) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; (n * self.secsize) as usize];
        self.file
            .seek(SeekFrom::Start(s * self.secsize))
            .and_then(|_| self.file.read_exact(&mut buf))
            .map_err(|e| format!("{}: read: {}", self.path, purgatorio::Error::from(e)))?;
        Ok(buf)
    }

    /// Writes whole sectors at sector `s`.
    pub fn write(&mut self, s: u64, buf: &[u8]) -> Result<(), String> {
        self.file
            .seek(SeekFrom::Start(s * self.secsize))
            .and_then(|_| self.file.write_all(buf))
            .map_err(|e| format!("{}: write: {}", self.path, purgatorio::Error::from(e)))
    }

    /// Sends control messages to the disk's `ctl` file, one write each.
    pub fn ctl_write(&self, msgs: &[String]) -> Result<(), String> {
        let Some(ctl) = &self.ctl else {
            return Ok(());
        };
        let mut f = OpenOptions::new()
            .write(true)
            .open(ctl)
            .map_err(|e| format!("{ctl}: {}", purgatorio::Error::from(e)))?;
        for m in msgs {
            f.write_all(m.as_bytes())
                .map_err(|e| format!("{ctl}: {}: {}", m.trim_end(), purgatorio::Error::from(e)))?;
        }
        Ok(())
    }
}

/// A partition being edited.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    /// The first sector, and one past the last.
    pub start: u64,
    pub end: u64,
    /// Changed since the table was read.
    pub changed: bool,
    /// fdisk's partition type and active flag.
    pub ptype: u8,
    pub active: bool,
}

/// What prep and fdisk do differently.
pub trait Scheme {
    /// Whether `name` may be the name of a new partition.
    fn check_name(&self, ed: &Edit, name: &str) -> Result<(), String>;
    /// A new partition (fdisk gives it a type).
    fn new_part(&self, name: &str, start: u64, end: u64) -> Part {
        Part {
            name: String::from(name),
            start,
            end,
            changed: true,
            ptype: 0,
            active: false,
        }
    }
    /// What `p` adds after a partition's size.
    fn describe(&self, _p: &Part) -> String {
        String::new()
    }
    /// The `ctl` messages for the table: what `P` and `-p` print.
    fn ctl_lines(&self, ed: &Edit) -> Vec<String>;
    /// Writes the table.
    fn write(&mut self, ed: &Edit, disk: &mut Disk) -> Result<(), String>;
    /// A command of this scheme's own; `None` if there is no such command.
    fn command(
        &mut self,
        _ed: &mut Edit,
        _cmd: &str,
        _args: &[&str],
    ) -> Option<Result<(), String>> {
        None
    }
    /// Help lines for the scheme's own commands.
    fn help(&self) -> &'static str {
        ""
    }
}

/// The partition table editor.
pub struct Edit {
    pub parts: Vec<Part>,
    /// `.`: the end of the last partition added.
    pub dot: u64,
    /// The first sector a partition may use, and the disk's size.
    pub lo: u64,
    pub secs: u64,
    /// Bytes per sector, for the sizes `p` prints.
    pub secsize: u64,
    pub changed: bool,
    /// Whether `w` is allowed.
    pub read_only: bool,
    warned: bool,
}

impl Edit {
    pub fn new(parts: Vec<Part>, lo: u64, secs: u64) -> Edit {
        Edit {
            parts,
            dot: lo,
            lo,
            secs,
            secsize: 512,
            changed: false,
            read_only: false,
            warned: false,
        }
    }

    /// The partitions in order of their start.
    pub fn sorted(&mut self) {
        self.parts.sort_by_key(|p| p.start);
    }

    /// Adds a partition, checking that it fits and overlaps no other.
    pub fn add(
        &mut self,
        scheme: &dyn Scheme,
        name: &str,
        start: u64,
        end: u64,
    ) -> Result<(), String> {
        scheme.check_name(self, name)?;
        if self.parts.iter().any(|p| p.name == name) {
            return Err(format!("already have partition with name \"{name}\""));
        }
        if start >= end {
            return Err(format!("start {start} not before end {end}"));
        }
        if start < self.lo || end > self.secs {
            return Err(format!(
                "partition {start} {end} not in usable range {} {}",
                self.lo, self.secs
            ));
        }
        if let Some(p) = self.parts.iter().find(|p| start < p.end && p.start < end) {
            return Err(format!(
                "{name} {start} {end} overlaps with {} {} {}",
                p.name, p.start, p.end
            ));
        }
        self.parts.push(scheme.new_part(name, start, end));
        self.sorted();
        self.dot = end;
        self.changed = true;
        Ok(())
    }

    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        let n = self.parts.len();
        self.parts.retain(|p| p.name != name);
        if self.parts.len() == n {
            return Err(format!("cannot find partition \"{name}\""));
        }
        self.changed = true;
        Ok(())
    }

    /// The largest end a partition starting at `start` can have.
    fn max_end(&self, start: u64) -> u64 {
        self.parts
            .iter()
            .filter(|p| p.start >= start)
            .map(|p| p.start)
            .min()
            .unwrap_or(self.secs)
    }

    /// The table as `p` prints it: each partition and each gap, with a
    /// `'` before a changed one.
    pub fn print(&self, scheme: &dyn Scheme) -> String {
        let mut out = String::new();
        let mut at = self.lo;
        let w = self.secs.to_string().len();
        let line = |out: &mut String, mark: char, name: &str, s: u64, e: u64, extra: &str| {
            out.push_str(&format!(
                "{mark} {name:<12} {s:>w$} {e:<w$} ({} sectors, {}){extra}\n",
                e - s,
                size((e - s) * self.secsize)
            ));
        };
        for p in &self.parts {
            if p.start > at {
                line(&mut out, ' ', "empty", at, p.start, "");
            }
            let mark = if p.changed { '\'' } else { ' ' };
            line(&mut out, mark, &p.name, p.start, p.end, &scheme.describe(p));
            at = at.max(p.end);
        }
        if at < self.secs {
            line(&mut out, ' ', "empty", at, self.secs, "");
        }
        out
    }

    /// Evaluates a sector expression: numbers, `.`, `$`, `n%` (of the
    /// disk), and `+ - * /` with the usual precedence.
    pub fn expr(&self, s: &str, dollar: u64) -> Result<u64, String> {
        let bad = || format!("bad expression \"{s}\"");
        let mut toks: Vec<Tok> = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b' ' | b'\t' => i += 1,
                b'0'..=b'9' => {
                    let j = b[i..]
                        .iter()
                        .position(|c| !c.is_ascii_digit())
                        .map_or(b.len(), |k| i + k);
                    let n: u64 = s[i..j].parse().map_err(|_| bad())?;
                    if b.get(j) == Some(&b'%') {
                        toks.push(Tok::Num((n as u128 * self.secs as u128 / 100) as u64));
                        i = j + 1;
                    } else {
                        toks.push(Tok::Num(n));
                        i = j;
                    }
                }
                b'.' => {
                    toks.push(Tok::Num(self.dot));
                    i += 1;
                }
                b'$' => {
                    toks.push(Tok::Num(dollar));
                    i += 1;
                }
                c @ (b'+' | b'-' | b'*' | b'/') => {
                    toks.push(Tok::Op(c));
                    i += 1;
                }
                _ => return Err(bad()),
            }
        }
        // A sum of products.
        let mut pos = 0;
        let term = |pos: &mut usize| -> Result<i128, String> {
            let mut v = match toks.get(*pos) {
                Some(Tok::Num(n)) => *n as i128,
                _ => return Err(bad()),
            };
            *pos += 1;
            while let Some(Tok::Op(op @ (b'*' | b'/'))) = toks.get(*pos) {
                let Some(Tok::Num(n)) = toks.get(*pos + 1) else {
                    return Err(bad());
                };
                v = if *op == b'*' {
                    v * *n as i128
                } else if *n == 0 {
                    return Err(String::from("division by zero"));
                } else {
                    v / *n as i128
                };
                *pos += 2;
            }
            Ok(v)
        };
        let mut v = term(&mut pos)?;
        while let Some(Tok::Op(op)) = toks.get(pos) {
            let op = *op;
            pos += 1;
            let t = term(&mut pos)?;
            v = if op == b'+' { v + t } else { v - t };
        }
        if pos != toks.len() {
            return Err(bad());
        }
        u64::try_from(v).map_err(|_| format!("negative value in \"{s}\""))
    }

    /// Runs the editor: commands from `input`, a `>>> ` prompt before
    /// each. Returns at `q` or the end of the input.
    pub fn run(&mut self, scheme: &mut dyn Scheme, disk: &mut Disk, input: &mut dyn BufRead) {
        loop {
            print!(">>> ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if input.read_line(&mut line).unwrap_or(0) == 0 {
                println!();
                return;
            }
            let f: Vec<&str> = line.split_whitespace().collect();
            let Some((&cmd, args)) = f.split_first() else {
                continue;
            };
            match self.command(scheme, disk, input, cmd, args) {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => println!("?{e}"),
            }
        }
    }

    /// One editor command; true to quit.
    fn command(
        &mut self,
        scheme: &mut dyn Scheme,
        disk: &mut Disk,
        input: &mut dyn BufRead,
        cmd: &str,
        args: &[&str],
    ) -> Result<bool, String> {
        if cmd != "q" {
            self.warned = false;
        }
        match cmd {
            "a" => {
                let Some(name) = args.first() else {
                    return Err(String::from("usage: a name [start [end]]"));
                };
                let mut ask = |what: &str, given: Option<&&str>| -> Result<String, String> {
                    if let Some(g) = given {
                        return Ok(String::from(*g));
                    }
                    print!("{what}: ");
                    let _ = std::io::stdout().flush();
                    let mut s = String::new();
                    input.read_line(&mut s).map_err(|e| e.to_string())?;
                    Ok(String::from(s.trim()))
                };
                let last = self.parts.iter().map(|p| p.end).max().unwrap_or(self.lo);
                let start = self.expr(&ask("start", args.get(1))?, last)?;
                let end_s = ask("end", args.get(2))?;
                let end = self.expr(&end_s, self.max_end(start))?;
                self.add(&*scheme, name, start, end)?;
            }
            "d" => {
                let [name] = args else {
                    return Err(String::from("usage: d name"));
                };
                self.delete(name)?;
            }
            "." => {
                let [e] = args else {
                    return Err(String::from("usage: . newdot"));
                };
                self.dot = self.expr(e, self.secs)?;
            }
            "p" => print!("{}", self.print(&*scheme)),
            "P" => {
                for l in scheme.ctl_lines(self) {
                    print!("{l}");
                }
            }
            "w" => {
                if self.read_only {
                    return Err(String::from("read only"));
                }
                scheme.write(self, disk)?;
                for p in &mut self.parts {
                    p.changed = false;
                }
                self.changed = false;
            }
            "q" => {
                if self.changed && !self.warned {
                    self.warned = true;
                    return Err(String::from("changes unwritten; type q again to quit"));
                }
                return Ok(true);
            }
            "h" => {
                print!(
                    ". newdot - set dot\n\
                     a name [start [end]] - add partition\n\
                     d name - delete partition\n\
                     h - print help message\n\
                     p - print partition table\n\
                     P - print commands to update sd(3) device\n\
                     w - write partition table\n\
                     q - quit\n{}",
                    scheme.help()
                );
            }
            _ => match scheme.command(self, cmd, args) {
                Some(r) => r?,
                None => return Err(format!("unknown command \"{cmd}\"; h for help")),
            },
        }
        Ok(false)
    }
}

enum Tok {
    Num(u64),
    Op(u8),
}

/// `b` bytes as a size with a unit, for people.
fn size(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{}.{:01} GB", b >> 30, ((b % (1 << 30)) * 10) >> 30)
    } else if b >= 1 << 20 {
        format!("{}.{:01} MB", b >> 20, ((b % (1 << 20)) * 10) >> 20)
    } else if b >= 1 << 10 {
        format!("{} KB", b >> 10)
    } else {
        format!("{b} B")
    }
}

/// Reads all of standard input's lines for the editor.
pub fn stdin() -> std::io::BufReader<std::io::Stdin> {
    std::io::BufReader::new(std::io::stdin())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;
    impl Scheme for Plain {
        fn check_name(&self, _ed: &Edit, _name: &str) -> Result<(), String> {
            Ok(())
        }
        fn ctl_lines(&self, _ed: &Edit) -> Vec<String> {
            Vec::new()
        }
        fn write(&mut self, _ed: &Edit, _disk: &mut Disk) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn expressions() {
        let mut ed = Edit::new(Vec::new(), 2, 1000);
        ed.dot = 100;
        assert_eq!(ed.expr("10", 0), Ok(10));
        assert_eq!(ed.expr(". + 20%", 0), Ok(300));
        assert_eq!(ed.expr("$-1", 50), Ok(49));
        assert_eq!(ed.expr("2+3*4", 0), Ok(14));
        assert_eq!(ed.expr("10/3*3", 0), Ok(9));
        assert!(ed.expr("1-5", 0).is_err());
        assert!(ed.expr("1+", 0).is_err());
        assert!(ed.expr("x", 0).is_err());
    }

    #[test]
    fn adding() {
        let mut ed = Edit::new(Vec::new(), 2, 1000);
        ed.add(&Plain, "a", 2, 100).unwrap();
        assert!(ed.add(&Plain, "b", 50, 200).is_err());
        assert!(ed.add(&Plain, "a", 300, 400).is_err());
        assert!(ed.add(&Plain, "c", 900, 1001).is_err());
        ed.add(&Plain, "c", 500, 1000).unwrap();
        assert_eq!(ed.max_end(100), 500);
        assert_eq!(ed.dot, 1000);
        ed.delete("a").unwrap();
        assert!(ed.delete("a").is_err());
    }
}
