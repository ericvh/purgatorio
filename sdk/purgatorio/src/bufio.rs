//! Buffered I/O in the style of Inferno's `bufio`: [`Iobuf`] reads by line
//! or delimiter on top of any `Read`, and writes are buffered until flushed
//! or dropped. Most apps can use `std::io::BufReader` directly; this adds
//! the Inferno operations `gets` (keep the delimiter) and `getc`.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};

use crate::Error;

pub struct Iobuf<R> {
    r: BufReader<R>,
}

impl<R: Read> Iobuf<R> {
    pub fn new(r: R) -> Iobuf<R> {
        Iobuf {
            r: BufReader::new(r),
        }
    }

    /// The bytes up to and including `delim`, or up to end of file. `None`
    /// at end of file.
    pub fn gets(&mut self, delim: u8) -> io::Result<Option<String>> {
        let mut buf = Vec::new();
        if self.r.read_until(delim, &mut buf)? == 0 {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// The next character, or `None` at end of file. Invalid UTF-8 reads as
    /// U+FFFD, one byte at a time.
    pub fn getc(&mut self) -> io::Result<Option<char>> {
        let first = match self.r.fill_buf()?.first() {
            Some(&b) => b,
            None => return Ok(None),
        };
        let len = match first {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 0,
        };
        let mut bytes = [0u8; 4];
        let mut n = 0;
        while n < len.max(1) {
            match self.r.fill_buf()?.first() {
                Some(&b) if n == 0 || b & 0xc0 == 0x80 => {
                    bytes[n] = b;
                    self.r.consume(1);
                    n += 1;
                }
                _ => break,
            }
        }
        Ok(Some(
            std::str::from_utf8(&bytes[..n])
                .ok()
                .and_then(|s| s.chars().next())
                .unwrap_or(char::REPLACEMENT_CHARACTER),
        ))
    }

    /// Lines without their newline.
    pub fn lines(self) -> impl Iterator<Item = io::Result<String>> {
        self.r.lines()
    }
}

impl<R: Read> Read for Iobuf<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.r.read(buf)
    }
}

/// Opens `path` for buffered reading.
pub fn open(path: &str) -> Result<Iobuf<File>, Error> {
    Ok(Iobuf::new(File::open(path)?))
}

/// Buffered standard output, flushed when dropped.
pub fn stdout() -> BufWriter<io::Stdout> {
    BufWriter::new(io::stdout())
}

/// Writes `s` and flushes; the Inferno `puts` plus `flush` pair.
pub fn puts(w: &mut impl Write, s: &str) -> io::Result<()> {
    w.write_all(s.as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::Iobuf;

    #[test]
    fn gets_and_getc() {
        let mut b = Iobuf::new("one\ntwo\nlast".as_bytes());
        assert_eq!(b.gets(b'\n').unwrap().as_deref(), Some("one\n"));
        assert_eq!(b.getc().unwrap(), Some('t'));
        assert_eq!(b.gets(b'\n').unwrap().as_deref(), Some("wo\n"));
        assert_eq!(b.gets(b'\n').unwrap().as_deref(), Some("last"));
        assert_eq!(b.gets(b'\n').unwrap(), None);
        let mut u = Iobuf::new("é\u{fffd}".as_bytes());
        assert_eq!(u.getc().unwrap(), Some('é'));
        assert_eq!(u.getc().unwrap(), Some('\u{fffd}'));
        assert_eq!(u.getc().unwrap(), None);
    }
}
