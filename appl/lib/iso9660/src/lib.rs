//! ISO 9660 images, read-only, as 9660srv(4) serves them: the primary
//! volume with Rock Ridge (RRIP) names and permissions when the image has
//! them, or the Joliet volume's Unicode names when there is one. Names
//! from the primary volume are lowercased and lose their `;1` version.
//!
//! [`build`] makes small images, for tests.

pub mod build;

use std::io::{Read, Seek, SeekFrom};

/// The size of a CD sector.
pub const SECTOR: u64 = 2048;

/// Where an image is read from.
pub trait Disk {
    /// Fills `buf` from byte `off`.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String>;
}

/// A file or device read with `Seek` and `Read`.
pub struct Image<R>(pub R);

impl<R: Read + Seek> Disk for Image<R> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .seek(SeekFrom::Start(off))
            .and_then(|_| self.0.read_exact(buf))
            .map_err(|e| e.to_string())
    }
}

impl Disk for &[u8] {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        let start = usize::try_from(off).map_err(|_| String::from(EIO))?;
        let src = start
            .checked_add(buf.len())
            .and_then(|end| self.get(start..end))
            .ok_or_else(|| String::from(EIO))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

const EIO: &str = "i/o error";
const EFORMAT: &str = "CD format not recognised";

/// Which kind of directory tree is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The primary volume, with names as ISO 9660 has them.
    Iso,
    /// The primary volume with Rock Ridge entries.
    Ridge,
    /// The Joliet supplementary volume.
    Joliet,
}

impl Format {
    /// The group 9660srv gives the files.
    pub fn group(self) -> &'static str {
        match self {
            Format::Iso => "iso",
            Format::Ridge => "ridge",
            Format::Joliet => "joliet",
        }
    }
}

/// What a volume may use: `rock` for Rock Ridge entries, `joliet` for a
/// Joliet volume.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub rock: bool,
    pub joliet: bool,
}

/// A file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    /// The permission bits: Rock Ridge's, or 0444 (0555 for a directory).
    pub perm: u32,
    /// Seconds since 1970, as the record gives the time (no time zone).
    pub mtime: u32,
    /// The byte offset of the data in the image.
    pub offset: u64,
    pub size: u64,
}

/// An opened volume.
#[derive(Debug, Clone)]
pub struct Volume {
    pub format: Format,
    block: u64,
    root: Entry,
    /// Bytes before the System Use entries of each record (SUSP `SP`).
    skip: usize,
}

fn le16(b: &[u8], at: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn le32(b: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]))
}

/// Seconds since 1970 for a 7-byte directory record date; 0 when it is
/// before 1970 or makes no sense.
pub fn rec_time(d: &[u8]) -> u32 {
    let (y, mo, da, h, mi, s) = (
        i64::from(d[0]) + 1900,
        i64::from(d[1]),
        i64::from(d[2]),
        i64::from(d[3]),
        i64::from(d[4]),
        i64::from(d[5]),
    );
    if y < 1970 || !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || s > 59
    {
        return 0;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u32::try_from(days * 86400 + h * 3600 + mi * 60 + s).unwrap_or(0)
}

/// A record's raw parts.
struct Rec<'a> {
    offset: u64,
    size: u64,
    dir: bool,
    mtime: u32,
    name: &'a [u8],
    su: &'a [u8],
}

fn parse_rec(r: &[u8]) -> Option<Rec<'_>> {
    if r.len() < 34 {
        return None;
    }
    let nl = usize::from(r[32]);
    let name = r.get(33..33 + nl)?;
    let su_start = 33 + nl + usize::from(nl.is_multiple_of(2));
    Some(Rec {
        offset: le32(r, 2),
        size: le32(r, 10),
        dir: r[25] & 2 != 0,
        mtime: rec_time(&r[18..25]),
        name,
        su: r.get(su_start..).unwrap_or(&[]),
    })
}

/// What a record's System Use entries say.
#[derive(Default)]
struct Susp {
    name: Option<String>,
    perm: Option<u32>,
    sp_skip: Option<usize>,
    rock: bool,
}

impl Volume {
    /// Reads the volume descriptors from sector 16 on.
    pub fn open(d: &mut dyn Disk, opt: Options) -> Result<Volume, String> {
        let mut found: Option<(Format, Vec<u8>)> = None;
        let mut buf = vec![0u8; SECTOR as usize];
        for s in 16..100u64 {
            if d.read_at(s * SECTOR, &mut buf).is_err() {
                break;
            }
            if &buf[1..6] != b"CD001" {
                break;
            }
            match buf[0] {
                1 => found = Some((Format::Iso, buf.clone())),
                2 if opt.joliet
                    && buf[88] == 0x25
                    && buf[89] == 0x2f
                    && matches!(buf[90], 0x40 | 0x43 | 0x45) =>
                {
                    found = Some((Format::Joliet, buf.clone()));
                }
                255 => break,
                _ => {}
            }
        }
        let (format, pvd) = found.ok_or_else(|| String::from(EFORMAT))?;
        let block = le16(&pvd, 128);
        if block == 0 || block > SECTOR {
            return Err(String::from("blocksize too big"));
        }
        let root = parse_rec(&pvd[156..190]).ok_or_else(|| String::from(EFORMAT))?;
        let mut v = Volume {
            format,
            block,
            root: Entry {
                name: String::from("/"),
                dir: true,
                perm: 0o555,
                mtime: root.mtime,
                offset: root.offset * block,
                size: root.size,
            },
            skip: 0,
        };
        if format == Format::Iso && opt.rock {
            // The root's `.` record says whether there are Rock Ridge
            // entries, and how far into each System Use area they start.
            let mut first = vec![0u8; SECTOR as usize];
            d.read_at(v.root.offset, &mut first)?;
            let len = usize::from(first[0]);
            if let Some(dot) = first.get(..len).and_then(parse_rec) {
                let s = v.susp(d, dot.su, 0)?;
                if s.rock {
                    v.format = Format::Ridge;
                    v.skip = s.sp_skip.unwrap_or(0);
                }
            }
        }
        Ok(v)
    }

    pub fn root(&self) -> Entry {
        self.root.clone()
    }

    /// The System Use entries of a record, following continuations.
    fn susp(&self, d: &mut dyn Disk, area: &[u8], skip: usize) -> Result<Susp, String> {
        let mut out = Susp::default();
        let mut name = String::new();
        let mut have_name = false;
        let mut cur = area.get(skip..).unwrap_or(&[]).to_vec();
        for _ in 0..16 {
            let mut next: Option<(u64, u64, usize)> = None;
            let mut at = 0;
            while at + 4 <= cur.len() {
                let e = &cur[at..];
                let l = usize::from(e[2]);
                if l < 4 || l > e.len() {
                    break;
                }
                let e = &e[..l];
                match &e[..2] {
                    b"SP" if l >= 7 && e[4] == 0xbe && e[5] == 0xef => {
                        out.sp_skip = Some(usize::from(e[6]));
                    }
                    b"RR" => out.rock = true,
                    b"ER" if l >= 8 => {
                        let idl = usize::from(e[4]);
                        let id = e.get(8..8 + idl).unwrap_or(&[]);
                        if matches!(id, b"RRIP_1991A" | b"IEEE_P1282" | b"IEEE_1282") {
                            out.rock = true;
                        }
                    }
                    b"CE" if l >= 28 => {
                        next = Some((le32(e, 4), le32(e, 12), le32(e, 20) as usize));
                    }
                    b"NM" if l >= 5 => {
                        // Not `.` or `..`.
                        if e[4] & 6 == 0 {
                            name.push_str(&String::from_utf8_lossy(&e[5..]));
                            have_name = true;
                        }
                    }
                    b"PX" if l >= 12 => out.perm = Some(le32(e, 4) as u32 & 0o777),
                    b"ST" => {
                        next = None;
                        at = cur.len();
                        continue;
                    }
                    _ => {}
                }
                at += l;
            }
            let Some((blk, off, len)) = next else {
                break;
            };
            let len = len.min(SECTOR as usize);
            cur = vec![0u8; len];
            d.read_at(blk * self.block + off, &mut cur)?;
        }
        if have_name {
            out.name = Some(name);
        }
        Ok(out)
    }

    fn name(&self, raw: &[u8]) -> String {
        let mut s = match self.format {
            Format::Joliet => {
                let units: Vec<u16> = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&units)
            }
            _ => raw
                .iter()
                .map(|&b| char::from(b).to_ascii_lowercase())
                .collect(),
        };
        if let Some(i) = s.find(';') {
            s.truncate(i);
        }
        s
    }

    /// The entries of the directory `dir`, without `.` and `..`.
    pub fn list(&self, d: &mut dyn Disk, dir: &Entry) -> Result<Vec<Entry>, String> {
        if !dir.dir {
            return Err(String::from("not a directory"));
        }
        let mut out = Vec::new();
        let mut data = vec![0u8; usize::try_from(dir.size).map_err(|_| String::from(EIO))?];
        d.read_at(dir.offset, &mut data)?;
        let bs = self.block as usize;
        let mut at = 0;
        while at < data.len() {
            let len = usize::from(data[at]);
            if len == 0 {
                // The rest of this block is padding.
                at = (at / bs + 1) * bs;
                continue;
            }
            let Some(r) = data.get(at..at + len).and_then(parse_rec) else {
                break;
            };
            at += len;
            if r.name == [0] || r.name == [1] {
                continue;
            }
            let mut name = self.name(r.name);
            let mut perm = if r.dir { 0o555 } else { 0o444 };
            if self.format == Format::Ridge {
                let s = self.susp(d, r.su, self.skip)?;
                if let Some(n) = s.name {
                    name = n;
                }
                if let Some(p) = s.perm {
                    perm = p;
                }
            }
            name.truncate(255);
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                continue;
            }
            out.push(Entry {
                name,
                dir: r.dir,
                perm,
                mtime: r.mtime,
                offset: r.offset * self.block,
                size: r.size,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::build::{File, build};
    use super::*;

    fn files() -> Vec<File> {
        vec![
            File::dir("docs", 0o750),
            File::regular("docs/readme.txt", 0o640, b"read me\n"),
            File::regular("A Long Mixed-Case Name.text", 0o600, b"long"),
            File::regular(&"n".repeat(150), 0o644, b"very long name"),
            File::regular("deep/er/x", 0o644, &vec![7u8; 5000]),
            File::regular("empty", 0o644, b""),
        ]
    }

    fn walk(img: &[u8], opt: Options) -> (Format, Vec<(String, u32, u64)>) {
        let mut d: &[u8] = img;
        let v = Volume::open(&mut d, opt).unwrap();
        let mut out = Vec::new();
        let mut stack = vec![(String::new(), v.root())];
        while let Some((path, e)) = stack.pop() {
            for k in v.list(&mut d, &e).unwrap() {
                let p = format!("{path}/{}", k.name);
                out.push((p.clone(), k.perm, k.size));
                if k.dir {
                    stack.push((p, k));
                }
            }
        }
        out.sort();
        (v.format, out)
    }

    #[test]
    fn rock_ridge() {
        let img = build(&files(), true, false);
        let (f, all) = walk(
            &img,
            Options {
                rock: true,
                joliet: true,
            },
        );
        assert_eq!(f, Format::Ridge);
        let names: Vec<&str> = all.iter().map(|e| e.0.as_str()).collect();
        let long = format!("/{}", "n".repeat(150));
        assert_eq!(
            names,
            [
                "/A Long Mixed-Case Name.text",
                "/deep",
                "/deep/er",
                "/deep/er/x",
                "/docs",
                "/docs/readme.txt",
                "/empty",
                long.as_str(),
            ]
        );
        assert_eq!(all[4].1, 0o750);
        assert_eq!(all[0].1, 0o600);
        assert_eq!(all[3].2, 5000);
        // Without Rock Ridge the ISO names show, lowercased.
        let (f, all) = walk(
            &img,
            Options {
                rock: false,
                joliet: true,
            },
        );
        assert_eq!(f, Format::Iso);
        assert!(
            all.iter()
                .any(|e| e.0 == "/docs/readme.txt" && e.1 == 0o444)
        );
        assert!(all.iter().any(|e| e.0 == "/docs" && e.1 == 0o555));
    }

    #[test]
    fn joliet() {
        let img = build(&files(), false, true);
        let (f, all) = walk(
            &img,
            Options {
                rock: true,
                joliet: true,
            },
        );
        assert_eq!(f, Format::Joliet);
        assert!(all.iter().any(|e| e.0 == "/A Long Mixed-Case Name.text"));
        let (f, all) = walk(
            &img,
            Options {
                rock: true,
                joliet: false,
            },
        );
        assert_eq!(f, Format::Iso);
        assert!(all.iter().any(|e| e.0 == "/deep/er/x"));
    }

    #[test]
    fn data_and_damage() {
        let img = build(&files(), true, false);
        let mut d: &[u8] = &img;
        let v = Volume::open(
            &mut d,
            Options {
                rock: true,
                joliet: true,
            },
        )
        .unwrap();
        let docs = v.list(&mut d, &v.root()).unwrap();
        let docs = docs.iter().find(|e| e.name == "docs").unwrap();
        let rd = &v.list(&mut d, docs).unwrap()[0];
        let mut buf = vec![0u8; rd.size as usize];
        d.read_at(rd.offset, &mut buf).unwrap();
        assert_eq!(buf, b"read me\n");
        assert!(rd.mtime > 1_600_000_000);
        let mut short: &[u8] = &img[..17 * 2048];
        assert!(
            Volume::open(
                &mut short,
                Options {
                    rock: true,
                    joliet: true
                }
            )
            .is_err()
        );
        let zero = vec![0u8; 40000];
        let mut z: &[u8] = &zero;
        assert_eq!(
            Volume::open(
                &mut z,
                Options {
                    rock: true,
                    joliet: true
                }
            )
            .unwrap_err(),
            EFORMAT
        );
        // Truncated directory data is an error, not a panic.
        for cut in [18 * 2048, 19 * 2048 + 100, img.len() - 1] {
            let mut t: &[u8] = &img[..cut];
            if let Ok(v) = Volume::open(
                &mut t,
                Options {
                    rock: true,
                    joliet: true,
                },
            ) {
                let _ = v.list(&mut t, &v.root());
            }
        }
    }

    #[test]
    fn times() {
        assert_eq!(rec_time(&[70, 1, 1, 0, 0, 0, 0]), 0);
        assert_eq!(rec_time(&[70, 1, 1, 0, 0, 1, 0]), 1);
        assert_eq!(rec_time(&[124, 2, 29, 12, 0, 0, 0]), 1_709_208_000);
        assert_eq!(rec_time(&[60, 1, 1, 0, 0, 0, 0]), 0);
        assert_eq!(rec_time(&[100, 13, 1, 0, 0, 0, 0]), 0);
    }
}
