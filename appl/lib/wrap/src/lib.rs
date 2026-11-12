//! What the install/ commands share, in Inferno's formats:
//!
//! - archives as disk/mkfs writes them ([`Hdr`], [`Reader`], [`put_hdr`],
//!   [`put_end`]): a line `name mode uid gid mtime length` (the mode in
//!   octal, names quoted as `%q` does) before each file's bytes, and a last
//!   line `end of archive`;
//! - packages ("wraps"): an archive that starts with `/wrap/name/stamp/`,
//!   holding `package` (empty: a base package) or `update` (the stamp of
//!   the package it updates, right-justified in 22 columns), `desc`,
//!   `proto`, `md5sum` (a line `name digest` per file, sorted) and, in an
//!   update, `remove` (a name a line). Installed, those files are the
//!   record of what is there ([`units`]);
//! - replica logs ([`Entry`]): a line `time gen verb path serverpath mode
//!   uid gid mtime length [digest]` per change, the verb `a`, `c`, `d` or
//!   `m`, and `-` for a server path that is the same as the path.

use std::io::{BufRead, Read, Write};

pub use proto::quote;

/// An archive header.
#[derive(Clone, Debug, PartialEq)]
pub struct Hdr {
    pub name: String,
    pub mode: u32,
    pub uid: String,
    pub gid: String,
    pub mtime: u32,
    pub length: u64,
}

pub fn put_hdr(w: &mut dyn Write, h: &Hdr) -> std::io::Result<()> {
    writeln!(
        w,
        "{} {:o} {} {} {} {}",
        quote(&h.name),
        h.mode,
        quote(&h.uid),
        quote(&h.gid),
        h.mtime,
        h.length
    )
}

pub fn put_end(w: &mut dyn Write) -> std::io::Result<()> {
    w.write_all(b"end of archive\n")
}

/// Reads an archive.
pub struct Reader<R: BufRead> {
    r: R,
    /// Bytes of the current file not yet read.
    left: u64,
}

impl<R: BufRead> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader { r, left: 0 }
    }

    /// The next header, skipping what is left of the file before; `None`
    /// at the end of the archive.
    pub fn next_hdr(&mut self) -> Result<Option<Hdr>, String> {
        self.skip()?;
        let mut line = String::new();
        let n = self.r.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(String::from("premature end of archive"));
        }
        let line = line.trim_end_matches('\n');
        if line == "end of archive" {
            return Ok(None);
        }
        let f = purgatorio::unquote_fields(line);
        let bad = || format!("bad archive header: {line}");
        let [name, mode, uid, gid, mtime, length] = f.as_slice() else {
            return Err(bad());
        };
        let h = Hdr {
            name: name.clone(),
            mode: u32::from_str_radix(mode, 8).map_err(|_| bad())?,
            uid: uid.clone(),
            gid: gid.clone(),
            mtime: mtime.parse().map_err(|_| bad())?,
            length: length.parse().map_err(|_| bad())?,
        };
        self.left = if h.mode & purgatorio::DMDIR != 0 {
            0
        } else {
            h.length
        };
        Ok(Some(h))
    }

    /// The bytes of the current file.
    pub fn data(&mut self) -> Result<Vec<u8>, String> {
        let mut v = vec![0u8; self.left as usize];
        self.r
            .read_exact(&mut v)
            .map_err(|_| String::from("premature end of archive"))?;
        self.left = 0;
        Ok(v)
    }

    fn skip(&mut self) -> Result<(), String> {
        if self.left > 0 {
            let n = std::io::copy(&mut (&mut self.r).take(self.left), &mut std::io::sink())
                .map_err(|e| e.to_string())?;
            if n != self.left {
                return Err(String::from("premature end of archive"));
            }
            self.left = 0;
        }
        Ok(())
    }
}

/// A stamp as the name of a package's directory under `/wrap/name`.
pub fn stamp(t: u32) -> String {
    t.to_string()
}

/// The wrap header of a package: its directories and `package` or
/// `update` and `desc`, then each of `files` (name under the stamp's
/// directory, contents).
#[allow(clippy::too_many_arguments)]
pub fn put_wrap(
    w: &mut dyn Write,
    name: &str,
    time: u32,
    desc: &str,
    update_of: Option<u32>,
    uid: &str,
    gid: &str,
    files: &[(&str, Vec<u8>)],
) -> std::io::Result<()> {
    let dir = |n: String| Hdr {
        name: n,
        mode: purgatorio::DMDIR | 0o775,
        uid: String::from(uid),
        gid: String::from(gid),
        mtime: time,
        length: 0,
    };
    let base = format!("/wrap/{name}/{}", stamp(time));
    put_hdr(w, &dir(String::from("/wrap")))?;
    put_hdr(w, &dir(format!("/wrap/{name}")))?;
    put_hdr(w, &dir(base.clone()))?;
    let mut all: Vec<(&str, Vec<u8>)> = Vec::new();
    match update_of {
        Some(t) => all.push(("update", format!("{t:>22}\n").into_bytes())),
        None => all.push(("package", Vec::new())),
    }
    if !desc.is_empty() {
        all.push(("desc", format!("{desc}\n").into_bytes()));
    }
    all.extend(files.iter().cloned());
    for (elem, data) in all {
        put_hdr(
            w,
            &Hdr {
                name: format!("{base}/{elem}"),
                mode: 0o444,
                uid: String::from(uid),
                gid: String::from(gid),
                mtime: time,
                length: data.len() as u64,
            },
        )?;
        w.write_all(&data)?;
    }
    Ok(())
}

/// One installed (or archived) package directory, `/wrap/name/stamp`.
#[derive(Clone, Debug, Default)]
pub struct Unit {
    pub stamp: u32,
    /// It holds `package`: a base package (with `update` too, a full
    /// update).
    pub full: bool,
    /// The stamp in `update`, if it is an update.
    pub update_of: Option<u32>,
    pub desc: String,
    /// `md5sum`: name and digest, sorted by name.
    pub md5: Vec<(String, String)>,
    pub remove: Vec<String>,
}

impl Unit {
    pub fn kind(&self) -> &'static str {
        match (self.full, self.update_of.is_some()) {
            (true, false) => "base",
            (true, true) => "full update",
            _ => "update",
        }
    }

    pub fn digest(&self, name: &str) -> Option<&str> {
        self.md5
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .map(|i| self.md5[i].1.as_str())
    }
}

/// Parses `md5sum`.
pub fn parse_md5(text: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| {
            let f = purgatorio::unquote_fields(l);
            match f.as_slice() {
                [n, d] => Some((n.clone(), d.clone())),
                _ => None,
            }
        })
        .collect();
    v.sort();
    v
}

/// The units of the package in `dir` (`/wrap/name` of some tree) that
/// make up what is installed: the newest base package and the updates
/// after it, oldest first. A directory without `md5sum` is ignored.
pub fn units(dir: &str) -> Vec<Unit> {
    let mut all = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return all;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(t) = name.parse::<u32>() else {
            continue;
        };
        let d = format!("{dir}/{name}");
        let read = |f: &str| std::fs::read_to_string(format!("{d}/{f}")).ok();
        let Some(md5) = read("md5sum") else {
            continue;
        };
        let full = purgatorio::stat(&format!("{d}/package")).is_ok();
        let update_of = read("update").and_then(|s| s.trim().parse().ok());
        if !full && update_of.is_none() {
            continue;
        }
        all.push(Unit {
            stamp: t,
            full,
            update_of,
            desc: read("desc").unwrap_or_default().trim_end().to_string(),
            md5: parse_md5(&md5),
            remove: read("remove")
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect(),
        });
    }
    all.sort_by_key(|u| u.stamp);
    let base = all.iter().filter(|u| u.full).map(|u| u.stamp).max();
    all.retain(|u| base.is_some_and(|b| u.stamp >= b));
    all
}

/// The installed digest of `name` in `units`, newest first, and whether a
/// later update removed it.
pub fn installed_digest<'a>(units: &'a [Unit], name: &str) -> Option<&'a str> {
    for u in units.iter().rev() {
        if u.remove.iter().any(|r| r == name) {
            return None;
        }
        if let Some(d) = u.digest(name) {
            return Some(d);
        }
    }
    None
}

/// A replica log entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub time: u32,
    pub seq: u32,
    pub verb: char,
    pub path: String,
    /// `None` when it is `path`.
    pub serverpath: Option<String>,
    pub mode: u32,
    pub uid: String,
    pub gid: String,
    pub mtime: u32,
    pub length: u64,
    pub digest: Option<String>,
}

impl Entry {
    pub fn parse(line: &str) -> Result<Entry, String> {
        let f = purgatorio::unquote_fields(line);
        let bad = || format!("bad log entry: {line}");
        if f.len() < 10 || f.len() > 11 {
            return Err(bad());
        }
        let verb = match f[2].as_str() {
            "a" => 'a',
            "c" => 'c',
            "d" => 'd',
            "m" => 'm',
            _ => return Err(bad()),
        };
        Ok(Entry {
            time: f[0].parse().map_err(|_| bad())?,
            seq: f[1].parse().map_err(|_| bad())?,
            verb,
            path: f[3].clone(),
            serverpath: (f[4] != "-").then(|| f[4].clone()),
            mode: u32::from_str_radix(&f[5], 8).map_err(|_| bad())?,
            uid: f[6].clone(),
            gid: f[7].clone(),
            mtime: f[8].parse().map_err(|_| bad())?,
            length: f[9].parse().map_err(|_| bad())?,
            digest: f.get(10).cloned(),
        })
    }

    pub fn line(&self) -> String {
        let mut s = format!(
            "{} {} {} {} {} {:o} {} {} {} {}",
            self.time,
            self.seq,
            self.verb,
            quote(&self.path),
            self.serverpath.as_deref().map_or(String::from("-"), quote),
            self.mode,
            quote(&self.uid),
            quote(&self.gid),
            self.mtime,
            self.length
        );
        if let Some(d) = &self.digest {
            s.push(' ');
            s.push_str(d);
        }
        s
    }

    /// The file the server holds for this entry, under `root`.
    pub fn server(&self, root: &str) -> String {
        proto::mkpath(root, self.serverpath.as_deref().unwrap_or(&self.path))
    }
}

/// The state a log leaves each path in: its last entry, unless that is a
/// deletion. Sorted by path.
pub fn state(entries: &[Entry]) -> Vec<Entry> {
    let mut m: std::collections::BTreeMap<String, Entry> = Default::default();
    for e in entries {
        if e.verb == 'd' {
            m.remove(&e.path);
        } else {
            m.insert(e.path.clone(), e.clone());
        }
    }
    m.into_values().collect()
}

/// Reads a log file; a missing file is an empty log.
pub fn read_log(path: &str) -> Result<Vec<Entry>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{path}: {}", purgatorio::Error::from(e))),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(Entry::parse)
        .collect()
}

pub fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// The MD5 digest of a file, in hex.
pub fn md5_file(path: &str) -> Result<String, String> {
    let data =
        std::fs::read(path).map_err(|e| format!("{path}: {}", purgatorio::Error::from(e)))?;
    Ok(hex(&md5(&data)))
}

/// MD5 (RFC 1321).
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    let (mut a, mut b, mut c, mut d) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        let (mut aa, mut bb, mut cc, mut dd) = (a, b, c, d);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((bb & cc) | (!bb & dd), i),
                16..=31 => ((dd & bb) | (!dd & cc), (5 * i + 1) % 16),
                32..=47 => (bb ^ cc ^ dd, (3 * i + 5) % 16),
                _ => (cc ^ (bb | !dd), (7 * i) % 16),
            };
            let f = f.wrapping_add(aa).wrapping_add(K[i]).wrapping_add(m[g]);
            aa = dd;
            dd = cc;
            cc = bb;
            bb = bb.wrapping_add(f.rotate_left(S[i]));
        }
        a = a.wrapping_add(aa);
        b = b.wrapping_add(bb);
        c = c.wrapping_add(cc);
        d = d.wrapping_add(dd);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a.to_le_bytes());
    out[4..8].copy_from_slice(&b.to_le_bytes());
    out[8..12].copy_from_slice(&c.to_le_bytes());
    out[12..16].copy_from_slice(&d.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_vectors() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn archives() {
        let mut out = Vec::new();
        let h = Hdr {
            name: String::from("/a b"),
            mode: 0o644,
            uid: String::from("u"),
            gid: String::from("g"),
            mtime: 7,
            length: 3,
        };
        put_hdr(&mut out, &h).unwrap();
        out.extend_from_slice(b"xyz");
        put_end(&mut out).unwrap();
        assert!(out.starts_with(b"'/a b' 644 u g 7 3\nxyz"));
        let mut r = Reader::new(&out[..]);
        assert_eq!(r.next_hdr().unwrap(), Some(h));
        assert_eq!(r.next_hdr().unwrap(), None);
    }

    #[test]
    fn log_entries() {
        let l = "100 2 a /lib/x - 664 inferno inf 99 12 abc";
        let e = Entry::parse(l).unwrap();
        assert_eq!(e.line(), l);
        assert_eq!(e.serverpath, None);
        assert!(Entry::parse("1 2 z a - 0 u g 0 0").is_err());
    }
}
