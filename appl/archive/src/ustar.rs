//! POSIX.1 ustar archives.
//!
//! The writer produces plain ustar: names up to 255 bytes split between the
//! `prefix` and `name` fields, numeric fields in octal (base-256 for sizes
//! that do not fit), and the archive padded to whole 10240-byte records.
//! The reader also accepts old V7 headers, GNU long names (`L`) and pax
//! `path`, `size` and `mtime` records, as GNU tar writes them.

use std::io::{self, Read, Write};

/// The tar block size.
pub const BLOCK: usize = 512;
/// Archives are padded to a multiple of this (20 blocks).
pub const RECORD: u64 = 20 * BLOCK as u64;

/// What an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// A hard link (`1`) or symbolic link (`2`); Inferno has neither.
    Link,
    /// Devices, fifos and anything else not extracted.
    Other,
}

/// One archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// The path, without the trailing `/` that marks a directory.
    pub name: String,
    /// Permission bits (`0o777`).
    pub mode: u32,
    pub size: u64,
    /// Seconds since the epoch.
    pub mtime: u64,
    pub kind: Kind,
    pub link: String,
    pub uname: String,
    pub gname: String,
}

impl Header {
    /// A header for a file or directory with no owner names.
    pub fn new(name: &str, kind: Kind, mode: u32, size: u64, mtime: u64) -> Header {
        Header {
            name: String::from(name),
            mode,
            size: if kind == Kind::File { size } else { 0 },
            mtime,
            kind,
            link: String::new(),
            uname: String::new(),
            gname: String::new(),
        }
    }

    /// Packs the header into one block.
    pub fn pack(&self) -> Result<[u8; BLOCK], &'static str> {
        let mut b = [0u8; BLOCK];
        let mut name = self.name.trim_end_matches('/').to_string();
        if self.kind == Kind::Dir {
            name.push('/');
        }
        let (prefix, rest) = split_name(&name).ok_or("name too long")?;
        b[..rest.len()].copy_from_slice(rest.as_bytes());
        octal(&mut b[100..108], u64::from(self.mode & 0o7777));
        octal(&mut b[108..116], 0);
        octal(&mut b[116..124], 0);
        number(&mut b[124..136], self.size);
        number(&mut b[136..148], self.mtime);
        b[156] = match self.kind {
            Kind::File => b'0',
            Kind::Dir => b'5',
            Kind::Link => b'2',
            Kind::Other => b'0',
        };
        put_str(&mut b[157..257], &self.link);
        b[257..263].copy_from_slice(b"ustar\0");
        b[263..265].copy_from_slice(b"00");
        put_str(&mut b[265..297], &self.uname);
        put_str(&mut b[297..329], &self.gname);
        octal(&mut b[329..337], 0);
        octal(&mut b[337..345], 0);
        b[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        b[148..156].fill(b' ');
        let sum: u32 = b.iter().map(|&c| u32::from(c)).sum();
        // Six digits, a NUL and the space already there.
        let s = format!("{sum:06o}\0");
        b[148..155].copy_from_slice(s.as_bytes());
        Ok(b)
    }
}

/// Splits a name into ustar `prefix` and `name` parts.
fn split_name(name: &str) -> Option<(&str, &str)> {
    if name.len() <= 100 {
        return Some(("", name));
    }
    // The split is at a '/', which is in neither part.
    let bytes = name.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'/' && i <= 155 && name.len() - i - 1 <= 100 && i + 1 < name.len() {
            return Some((&name[..i], &name[i + 1..]));
        }
    }
    None
}

fn put_str(field: &mut [u8], s: &str) {
    let n = s.len().min(field.len() - 1);
    field[..n].copy_from_slice(&s.as_bytes()[..n]);
}

/// Zero-padded octal ending in a NUL.
fn octal(field: &mut [u8], v: u64) {
    let w = field.len() - 1;
    let s = format!("{v:0w$o}");
    field[..w].copy_from_slice(&s.as_bytes()[s.len() - w..]);
    field[w] = 0;
}

/// Octal when it fits, otherwise GNU base-256.
fn number(field: &mut [u8], v: u64) {
    let w = field.len() - 1;
    if v < 1u64 << (3 * w) {
        octal(field, v);
    } else {
        field.fill(0);
        let n = field.len();
        field[n - 8..].copy_from_slice(&v.to_be_bytes());
        field[0] = 0x80;
    }
}

fn parse_number(field: &[u8]) -> Result<u64, &'static str> {
    if field.first().is_some_and(|&c| c & 0x80 != 0) {
        let mut v: u64 = u64::from(field[0] & 0x7f);
        for &c in &field[1..] {
            v = v.checked_mul(256).ok_or("bad number")? | u64::from(c);
        }
        return Ok(v);
    }
    let mut v: u64 = 0;
    let mut seen = false;
    for &c in field {
        match c {
            b'0'..=b'7' => {
                v = v.checked_mul(8).ok_or("bad number")? + u64::from(c - b'0');
                seen = true;
            }
            b' ' if !seen => {}
            b' ' | 0 => break,
            _ => return Err("bad number"),
        }
    }
    Ok(v)
}

fn cstr(field: &[u8]) -> String {
    let n = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..n]).into_owned()
}

/// The fields of one header block, before long-name records are applied.
struct Raw {
    typeflag: u8,
    header: Header,
}

const CHECKSUM: &str = "directory checksum error";

fn unpack(b: &[u8; BLOCK]) -> Result<Raw, &'static str> {
    let want = parse_number(&b[148..156]).map_err(|_| CHECKSUM)?;
    let mut sum: u64 = 0;
    let mut ssum: i64 = 0;
    for (i, &c) in b.iter().enumerate() {
        let c = if (148..156).contains(&i) { b' ' } else { c };
        sum += u64::from(c);
        ssum += i64::from(c as i8);
    }
    if want != sum && want as i64 != ssum {
        return Err(CHECKSUM);
    }
    let ustar = &b[257..263] == b"ustar\0";
    let mut name = cstr(&b[..100]);
    let prefix = cstr(&b[345..500]);
    if ustar && !prefix.is_empty() {
        name = format!("{prefix}/{name}");
    }
    let typeflag = b[156];
    let kind = match typeflag {
        b'0' | 0 | b'7' if name.ends_with('/') => Kind::Dir,
        b'0' | 0 | b'7' => Kind::File,
        b'5' => Kind::Dir,
        b'1' | b'2' => Kind::Link,
        _ => Kind::Other,
    };
    Ok(Raw {
        typeflag,
        header: Header {
            name,
            mode: parse_number(&b[100..108])? as u32 & 0o7777,
            size: parse_number(&b[124..136])?,
            mtime: parse_number(&b[136..148])?,
            kind,
            link: cstr(&b[157..257]),
            uname: cstr(&b[265..297]),
            gname: cstr(&b[297..329]),
        },
    })
}

/// Fields from pax extended headers that override the next entry.
#[derive(Default)]
struct Pending {
    name: Option<String>,
    size: Option<u64>,
    mtime: Option<u64>,
}

fn parse_pax(data: &[u8], p: &mut Pending) {
    let mut rest = data;
    while !rest.is_empty() {
        let Some(sp) = rest.iter().position(|&c| c == b' ') else {
            return;
        };
        let Some(len) = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
        else {
            return;
        };
        if len <= sp + 1 || len > rest.len() {
            return;
        }
        let rec = &rest[sp + 1..len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&c| c == b'=') {
            let v = String::from_utf8_lossy(&rec[eq + 1..]).into_owned();
            match &rec[..eq] {
                b"path" => p.name = Some(v),
                b"size" => p.size = v.parse().ok(),
                b"mtime" => p.mtime = v.split('.').next().and_then(|s| s.parse().ok()),
                _ => {}
            }
        }
        rest = &rest[len..];
    }
}

/// Reads archive entries in order.
pub struct Reader<R: Read> {
    r: R,
    /// Data bytes, including block padding, of the current entry not yet
    /// read.
    left: u64,
    /// Of `left`, how many are the entry's data rather than padding.
    data: u64,
}

fn padded(n: u64) -> u64 {
    n.div_ceil(BLOCK as u64) * BLOCK as u64
}

impl<R: Read> Reader<R> {
    pub fn new(r: R) -> Reader<R> {
        Reader {
            r,
            left: 0,
            data: 0,
        }
    }

    fn block(&mut self) -> Result<Option<[u8; BLOCK]>, String> {
        let mut b = [0u8; BLOCK];
        let mut got = 0;
        while got < BLOCK {
            match self.r.read(&mut b[got..]) {
                Ok(0) if got == 0 => return Ok(None),
                Ok(0) => return Err(String::from("unexpected end of archive")),
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(purgatorio::Error::from(e).0),
            }
        }
        Ok(Some(b))
    }

    fn skip(&mut self) -> Result<(), String> {
        while self.left > 0 {
            if self.block()?.is_none() {
                return Err(String::from("unexpected end of archive"));
            }
            self.left -= BLOCK as u64;
        }
        self.data = 0;
        Ok(())
    }

    fn slurp(&mut self, size: u64) -> Result<Vec<u8>, String> {
        let mut v = Vec::new();
        let mut left = padded(size);
        while left > 0 {
            let b = self.block()?.ok_or("unexpected end of archive")?;
            v.extend_from_slice(&b);
            left -= BLOCK as u64;
        }
        v.truncate(size as usize);
        Ok(v)
    }

    /// The next entry, skipping any unread data of the previous one, or
    /// `None` at the end of the archive.
    pub fn next_entry(&mut self) -> Result<Option<Header>, String> {
        self.skip()?;
        let mut pending = Pending::default();
        loop {
            let Some(b) = self.block()? else {
                return Ok(None);
            };
            if b.iter().all(|&c| c == 0) {
                return Ok(None);
            }
            let raw = unpack(&b).map_err(String::from)?;
            let meta = raw.header.size;
            match raw.typeflag {
                b'L' => {
                    let v = self.slurp(meta)?;
                    pending.name = Some(cstr(&v));
                    continue;
                }
                b'K' | b'g' => {
                    self.slurp(meta)?;
                    continue;
                }
                b'x' => {
                    let v = self.slurp(meta)?;
                    parse_pax(&v, &mut pending);
                    continue;
                }
                _ => {}
            }
            let mut h = raw.header;
            if let Some(s) = pending.size {
                h.size = s;
            }
            if let Some(n) = pending.name {
                h.name = n;
            }
            if let Some(m) = pending.mtime {
                h.mtime = m;
            }
            if h.name.ends_with('/') && h.kind == Kind::File {
                h.kind = Kind::Dir;
            }
            while h.name.len() > 1 && h.name.ends_with('/') {
                h.name.pop();
            }
            // Only regular files carry data that matters, but every entry's
            // size is skipped.
            self.left = padded(h.size);
            self.data = h.size;
            if h.kind == Kind::Dir {
                h.size = 0;
            }
            return Ok(Some(h));
        }
    }

    /// Copies the current entry's data to `out`.
    pub fn copy_data(&mut self, out: &mut dyn Write) -> Result<(), String> {
        while self.left > 0 {
            let b = self.block()?.ok_or("unexpected end of archive")?;
            self.left -= BLOCK as u64;
            let n = self.data.min(BLOCK as u64) as usize;
            self.data -= n as u64;
            out.write_all(&b[..n])
                .map_err(|e| purgatorio::Error::from(e).0)?;
        }
        Ok(())
    }
}

/// Writes an archive.
pub struct Writer<W: Write> {
    w: W,
    written: u64,
    /// Data bytes still owed for the current entry.
    owed: u64,
}

impl<W: Write> Writer<W> {
    pub fn new(w: W) -> Writer<W> {
        Writer {
            w,
            written: 0,
            owed: 0,
        }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.written += b.len() as u64;
        Ok(())
    }

    /// Starts an entry. Exactly `h.size` bytes of data must follow, through
    /// [`Writer::data`], before the next entry.
    pub fn header(&mut self, h: &Header) -> Result<(), String> {
        let b = h.pack().map_err(String::from)?;
        self.put(&b).map_err(|e| purgatorio::Error::from(e).0)?;
        self.owed = h.size;
        Ok(())
    }

    /// Writes entry data; anything past the header's size is dropped.
    pub fn data(&mut self, b: &[u8]) -> io::Result<()> {
        let n = (b.len() as u64).min(self.owed) as usize;
        self.owed -= n as u64;
        self.put(&b[..n])
    }

    /// Ends the entry: zero-fills data the file no longer had and pads to a
    /// block.
    pub fn end_entry(&mut self) -> io::Result<()> {
        let zero = [0u8; BLOCK];
        while self.owed > 0 {
            let n = self.owed.min(BLOCK as u64) as usize;
            self.owed -= n as u64;
            self.put(&zero[..n])?;
        }
        let pad = (padded(self.written) - self.written) as usize;
        self.put(&zero[..pad])
    }

    /// Writes the two zero blocks that end an archive, pads it to a whole
    /// record, and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.end_entry()?;
        let zero = [0u8; BLOCK];
        self.put(&zero)?;
        self.put(&zero)?;
        while !self.written.is_multiple_of(RECORD) {
            self.put(&zero)?;
        }
        self.w.flush()?;
        Ok(self.w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(Header, &[u8])]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new());
        for (h, d) in entries {
            w.header(h).unwrap();
            w.data(d).unwrap();
            w.end_entry().unwrap();
        }
        w.finish().unwrap()
    }

    #[test]
    fn round_trip() {
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(90));
        let entries = [
            (
                Header::new("dir", Kind::Dir, 0o755, 0, 1_000_000_000),
                &b""[..],
            ),
            (
                Header::new("dir/a", Kind::File, 0o644, 6, 1_000_000_001),
                b"hello\n",
            ),
            (Header::new(&long, Kind::File, 0o600, 0, 5), b""),
        ];
        let a = archive(&entries);
        assert_eq!(a.len() as u64 % RECORD, 0);
        // GNU tar would see `dir/` and the split name.
        assert_eq!(&a[..4], b"dir/");
        let mut r = Reader::new(&a[..]);
        for (want, data) in &entries {
            let h = r.next_entry().unwrap().unwrap();
            assert_eq!(&h, want);
            let mut got = Vec::new();
            r.copy_data(&mut got).unwrap();
            assert_eq!(&got, data);
        }
        assert!(r.next_entry().unwrap().is_none());
    }

    #[test]
    fn short_file_is_zero_filled() {
        let mut w = Writer::new(Vec::new());
        w.header(&Header::new("f", Kind::File, 0o644, 4, 0))
            .unwrap();
        w.data(b"ab").unwrap();
        w.end_entry().unwrap();
        let a = w.finish().unwrap();
        let mut r = Reader::new(&a[..]);
        r.next_entry().unwrap().unwrap();
        let mut got = Vec::new();
        r.copy_data(&mut got).unwrap();
        assert_eq!(got, b"ab\0\0");
    }

    #[test]
    fn too_long() {
        let h = Header::new(&"x".repeat(300), Kind::File, 0o644, 0, 0);
        assert_eq!(h.pack(), Err("name too long"));
    }

    #[test]
    fn bad_checksum() {
        let mut a = archive(&[(Header::new("f", Kind::File, 0o644, 0, 0), b"")]);
        a[0] = b'g';
        let mut r = Reader::new(&a[..]);
        assert_eq!(r.next_entry(), Err(String::from(CHECKSUM)));
    }

    #[test]
    fn gnu_long_name_and_pax() {
        let long = "n".repeat(150);
        let mut a = Vec::new();
        let mut l = Header::new("././@LongLink", Kind::File, 0, long.len() as u64 + 1, 0)
            .pack()
            .unwrap();
        l[156] = b'L';
        l[148..156].fill(b' ');
        let sum: u32 = l.iter().map(|&c| u32::from(c)).sum();
        l[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        a.extend_from_slice(&l);
        let mut d = long.clone().into_bytes();
        d.resize(BLOCK, 0);
        a.extend_from_slice(&d);
        a.extend(archive(&[(
            Header::new("short", Kind::File, 0o644, 1, 0),
            b"x",
        )]));
        let mut r = Reader::new(&a[..]);
        assert_eq!(r.next_entry().unwrap().unwrap().name, long);

        let mut p = Pending::default();
        parse_pax(b"30 mtime=1234567890.123456789\n14 path=a/b/c\n", &mut p);
        assert_eq!(p.name.as_deref(), Some("a/b/c"));
        assert_eq!(p.mtime, Some(1_234_567_890));
    }

    #[test]
    fn numbers() {
        let mut f = [0u8; 12];
        number(&mut f, 0o777);
        assert_eq!(&f, b"00000000777\0");
        assert_eq!(parse_number(&f), Ok(0o777));
        number(&mut f, 1 << 40);
        assert_eq!(parse_number(&f), Ok(1 << 40));
        assert_eq!(parse_number(b"   644 \0"), Ok(0o644));
    }
}
