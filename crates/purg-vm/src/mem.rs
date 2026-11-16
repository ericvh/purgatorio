//! Bounds-checked access to guest linear memory. Every guest pointer goes
//! through here; nothing is cast, so alignment never matters.

use purg_abi::error;
use purg_core::Error;

/// An out-of-bounds guest pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault;

impl From<Fault> for Error {
    fn from(_: Fault) -> Error {
        Error::new(error::EBADADDR)
    }
}

/// A guest pointer or size: `i32` in a wasm32 guest, `i64` in a wasm64
/// one. Host functions that take pointers are generic over it, so one
/// body serves both widths (`docs/wasm64.md`).
pub trait Ptr: Copy {
    /// Whether this is the 64-bit width.
    const WIDE: bool;
    /// The value, zero-extended.
    fn addr(self) -> u64;
    /// The pointer for an address, truncated to the width.
    fn at(addr: u64) -> Self;
    /// The pointer `n` bytes on, wrapping at the width.
    fn off(self, n: u64) -> Self {
        Self::at(self.addr().wrapping_add(n))
    }
    /// The value as a host size; one too large for the host is `usize::MAX`,
    /// which no memory holds.
    fn size(self) -> usize {
        usize::try_from(self.addr()).unwrap_or(usize::MAX)
    }
}

impl Ptr for i32 {
    const WIDE: bool = false;
    fn addr(self) -> u64 {
        u64::from(self as u32)
    }
    fn at(addr: u64) -> i32 {
        addr as u32 as i32
    }
}

impl Ptr for i64 {
    const WIDE: bool = true;
    fn addr(self) -> u64 {
        self as u64
    }
    fn at(addr: u64) -> i64 {
        addr as i64
    }
}

/// A guest's linear memory.
pub struct Mem<'a>(pub &'a mut [u8]);

fn range<P: Ptr>(ptr: P, len: P) -> Result<core::ops::Range<usize>, Fault> {
    let start = usize::try_from(ptr.addr()).map_err(|_| Fault)?;
    let len = usize::try_from(len.addr()).map_err(|_| Fault)?;
    Ok(start..start.checked_add(len).ok_or(Fault)?)
}

impl Mem<'_> {
    pub fn slice<P: Ptr>(&self, ptr: P, len: P) -> Result<&[u8], Fault> {
        self.0.get(range(ptr, len)?).ok_or(Fault)
    }

    pub fn slice_mut<P: Ptr>(&mut self, ptr: P, len: P) -> Result<&mut [u8], Fault> {
        self.0.get_mut(range(ptr, len)?).ok_or(Fault)
    }

    /// A UTF-8 string argument. Invalid UTF-8 is a bad argument, not a fault.
    pub fn str<P: Ptr>(&self, ptr: P, len: P) -> Result<&str, Error> {
        core::str::from_utf8(self.slice(ptr, len)?).map_err(|_| Error::new(error::EBADARG))
    }

    pub fn write<P: Ptr>(&mut self, ptr: P, data: &[u8]) -> Result<(), Fault> {
        let start = usize::try_from(ptr.addr()).map_err(|_| Fault)?;
        let end = start.checked_add(data.len()).ok_or(Fault)?;
        self.0
            .get_mut(start..end)
            .ok_or(Fault)?
            .copy_from_slice(data);
        Ok(())
    }

    pub fn u32<P: Ptr>(&self, ptr: P) -> Result<u32, Fault> {
        let b = self.slice(ptr, P::at(4))?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64<P: Ptr>(&self, ptr: P) -> Result<u64, Fault> {
        let b = self.slice(ptr, P::at(8))?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn put_u8<P: Ptr>(&mut self, ptr: P, v: u8) -> Result<(), Fault> {
        self.write(ptr, &[v])
    }

    pub fn put_u16<P: Ptr>(&mut self, ptr: P, v: u16) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }

    pub fn put_u32<P: Ptr>(&mut self, ptr: P, v: u32) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }

    pub fn put_u64<P: Ptr>(&mut self, ptr: P, v: u64) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_widths_reach_the_same_bytes() {
        let mut bytes = [0u8; 16];
        let mut mem = Mem(&mut bytes);
        mem.put_u32(4i32, 0xdead_beef).unwrap();
        assert_eq!(mem.u32(4i64), Ok(0xdead_beef));
        mem.put_u64(8i64, 7).unwrap();
        assert_eq!(mem.u64(8i32), Ok(7));
        assert_eq!(mem.slice(12i64, 4i64).map(<[u8]>::len), Ok(4));
    }

    #[test]
    fn out_of_range_faults() {
        let mut bytes = [0u8; 16];
        let mut mem = Mem(&mut bytes);
        assert_eq!(mem.slice(12i32, 5i32).err(), Some(Fault));
        assert_eq!(mem.slice(-1i32, 1i32).err(), Some(Fault));
        assert_eq!(mem.slice(-1i64, 1i64).err(), Some(Fault));
        assert_eq!(mem.slice(1i64, -1i64).err(), Some(Fault));
        assert_eq!(mem.slice(1i64 << 32, 1i64).err(), Some(Fault));
        assert_eq!(mem.write(15i64, &[1, 2]).err(), Some(Fault));
        assert_eq!(mem.u32(13i64).err(), Some(Fault));
    }

    #[test]
    fn offsets_wrap_at_the_width() {
        assert_eq!((-4i32).off(8), 4);
        assert_eq!(4i64.off(8), 12);
        assert_eq!(i32::at(1 << 32), 0);
    }
}
