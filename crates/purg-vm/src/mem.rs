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

/// A guest's linear memory.
pub struct Mem<'a>(pub &'a mut [u8]);

fn range(ptr: i32, len: i32) -> Result<core::ops::Range<usize>, Fault> {
    let start = ptr as u32 as usize;
    let len = len as u32 as usize;
    Ok(start..start.checked_add(len).ok_or(Fault)?)
}

impl Mem<'_> {
    pub fn slice(&self, ptr: i32, len: i32) -> Result<&[u8], Fault> {
        self.0.get(range(ptr, len)?).ok_or(Fault)
    }

    pub fn slice_mut(&mut self, ptr: i32, len: i32) -> Result<&mut [u8], Fault> {
        self.0.get_mut(range(ptr, len)?).ok_or(Fault)
    }

    /// A UTF-8 string argument. Invalid UTF-8 is a bad argument, not a fault.
    pub fn str(&self, ptr: i32, len: i32) -> Result<&str, Error> {
        core::str::from_utf8(self.slice(ptr, len)?).map_err(|_| Error::new(error::EBADARG))
    }

    pub fn write(&mut self, ptr: i32, data: &[u8]) -> Result<(), Fault> {
        self.slice_mut(ptr, data.len() as i32)?
            .copy_from_slice(data);
        Ok(())
    }

    pub fn u32(&self, ptr: i32) -> Result<u32, Fault> {
        let b = self.slice(ptr, 4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn put_u8(&mut self, ptr: i32, v: u8) -> Result<(), Fault> {
        self.write(ptr, &[v])
    }

    pub fn put_u16(&mut self, ptr: i32, v: u16) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }

    pub fn put_u32(&mut self, ptr: i32, v: u32) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }

    pub fn put_u64(&mut self, ptr: i32, v: u64) -> Result<(), Fault> {
        self.write(ptr, &v.to_le_bytes())
    }
}
