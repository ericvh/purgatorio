//! Little-endian field readers and writers. Every read is bounds-checked
//! and decodes with `from_le_bytes`.

use alloc::vec::Vec;

use crate::error::MESSAGE;

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        if n > self.buf.len() {
            return Err(MESSAGE);
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let b = self.bytes(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(b);
        Ok(a)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, &'static str> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, &'static str> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// `s[2]`: a u16 length and that many bytes.
    pub(crate) fn data16(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u16()? as usize;
        self.bytes(n)
    }

    /// `[count u32]`: a u32 length and that many bytes.
    pub(crate) fn data32(&mut self) -> Result<&'a [u8], &'static str> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }

    /// `s[2]` holding UTF-8.
    pub(crate) fn str16(&mut self) -> Result<&'a str, &'static str> {
        core::str::from_utf8(self.data16()?).map_err(|_| MESSAGE)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.buf)
    }

    /// Fails unless everything was read.
    pub(crate) fn finish(&self) -> Result<(), &'static str> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(MESSAGE)
        }
    }
}

pub(crate) struct Writer<'a> {
    pub(crate) buf: &'a mut Vec<u8>,
}

impl Writer<'_> {
    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// `s[2]`. Longer data is cut at 65535 bytes; callers check lengths
    /// first, so that never happens on the wire.
    pub(crate) fn data16(&mut self, b: &[u8]) {
        let n = b.len().min(u16::MAX as usize);
        self.u16(n as u16);
        self.bytes(&b[..n]);
    }

    pub(crate) fn data32(&mut self, b: &[u8]) {
        let n = b.len().min(u32::MAX as usize);
        self.u32(n as u32);
        self.bytes(&b[..n]);
    }
}
