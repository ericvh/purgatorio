use alloc::vec::Vec;

use crate::Error;

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.buf.len() < n {
            return Err(Error::Short);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.bytes(N)?.try_into().expect("length checked"))
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn string(&mut self) -> Result<&'a str, Error> {
        let n = self.u16()? as usize;
        core::str::from_utf8(self.bytes(n)?).map_err(|_| Error::BadString)
    }

    /// Everything left.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.buf)
    }

    pub(crate) fn finish(&self) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(Error::Trailing)
        }
    }
}

pub(crate) struct Writer<'v> {
    pub(crate) buf: &'v mut Vec<u8>,
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

    pub(crate) fn len16(&mut self, n: usize) -> Result<(), Error> {
        self.u16(u16::try_from(n).map_err(|_| Error::Overflow)?);
        Ok(())
    }

    pub(crate) fn string(&mut self, s: &str) -> Result<(), Error> {
        self.len16(s.len())?;
        self.buf.extend_from_slice(s.as_bytes());
        Ok(())
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
}
