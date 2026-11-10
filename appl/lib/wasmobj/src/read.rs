//! Checked reads of the binary format's primitives and of one entry of
//! each section.

use alloc::vec::Vec;

use crate::{
    Error, Export, FuncType, Global, GlobalType, Import, ImportType, Kind, Limits, Result, Table,
    ValType,
};

/// A cursor over a byte string. Offsets in errors count from the start of
/// that string plus `base`.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], pos: usize) -> Reader<'a> {
        Reader {
            buf,
            pos: pos.min(buf.len()),
        }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub fn err<T>(&self, what: &'static str) -> Result<T> {
        Err(Error {
            offset: self.pos,
            what,
        })
    }

    pub fn byte(&mut self) -> Result<u8> {
        match self.buf.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                Ok(b)
            }
            None => self.err("truncated module"),
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        match self
            .pos
            .checked_add(n)
            .and_then(|e| self.buf.get(self.pos..e))
        {
            Some(s) => {
                self.pos += n;
                Ok(s)
            }
            None => self.err("truncated module"),
        }
    }

    /// An unsigned LEB128 number of at most `bits` bits.
    fn leb(&mut self, bits: u32) -> Result<u64> {
        let start = self.pos;
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            if shift >= bits || (bits - shift < 7 && low >> (bits - shift) != 0) {
                self.pos = start;
                return self.err("bad LEB128 number");
            }
            v |= low << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn u32(&mut self) -> Result<u32> {
        self.leb(32).map(|v| v as u32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        self.leb(64)
    }

    /// A signed LEB128 number of at most 64 bits. Redundant sign bytes are
    /// accepted.
    pub fn s64(&mut self) -> Result<i64> {
        let start = self.pos;
        let mut v = 0i64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift >= 70 {
                self.pos = start;
                return self.err("bad LEB128 number");
            }
            if shift < 64 {
                v |= i64::from(b & 0x7f) << shift;
            }
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && b & 0x40 != 0 {
                    v |= -1i64 << shift;
                }
                return Ok(v);
            }
        }
    }

    /// A length-prefixed UTF-8 name.
    pub fn name(&mut self) -> Result<&'a str> {
        let n = self.u32()? as usize;
        let start = self.pos;
        let s = self.bytes(n)?;
        core::str::from_utf8(s).map_err(|_| Error {
            offset: start,
            what: "name is not UTF-8",
        })
    }

    /// A count followed by that many items.
    pub fn vec<T>(&mut self, mut item: impl FnMut(&mut Reader<'a>) -> Result<T>) -> Result<Vec<T>> {
        let n = self.u32()? as usize;
        // Every item takes at least a byte, so a count beyond what is left
        // is wrong; this also bounds the allocation.
        if n > self.buf.len() - self.pos {
            return self.err("count exceeds section");
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(item(self)?);
        }
        Ok(out)
    }
}

pub fn valtype(r: &mut Reader<'_>) -> Result<ValType> {
    let at = r.pos();
    Ok(match r.byte()? {
        0x7f => ValType::I32,
        0x7e => ValType::I64,
        0x7d => ValType::F32,
        0x7c => ValType::F64,
        0x7b => ValType::V128,
        0x70 => ValType::FuncRef,
        0x6f => ValType::ExternRef,
        _ => {
            return Err(Error {
                offset: at,
                what: "unsupported value type",
            });
        }
    })
}

pub fn functype(r: &mut Reader<'_>) -> Result<FuncType> {
    if r.byte()? != 0x60 {
        return Err(Error {
            offset: r.pos() - 1,
            what: "unsupported type form",
        });
    }
    Ok(FuncType {
        params: r.vec(valtype)?,
        results: r.vec(valtype)?,
    })
}

pub fn limits(r: &mut Reader<'_>) -> Result<Limits> {
    let flags = r.byte()?;
    if flags > 7 {
        return Err(Error {
            offset: r.pos() - 1,
            what: "bad limits",
        });
    }
    let wide = flags & 4 != 0;
    let num = |r: &mut Reader<'_>| {
        if wide {
            r.u64()
        } else {
            r.u32().map(u64::from)
        }
    };
    let min = num(r)?;
    let max = if flags & 1 != 0 { Some(num(r)?) } else { None };
    Ok(Limits {
        min,
        max,
        shared: flags & 2 != 0,
        wide,
    })
}

pub fn table(r: &mut Reader<'_>) -> Result<Table> {
    Ok(Table {
        elem: valtype(r)?,
        limits: limits(r)?,
    })
}

fn globaltype(r: &mut Reader<'_>) -> Result<GlobalType> {
    let val = valtype(r)?;
    let mutable = match r.byte()? {
        0 => false,
        1 => true,
        _ => {
            return Err(Error {
                offset: r.pos() - 1,
                what: "bad global type",
            });
        }
    };
    Ok(GlobalType { val, mutable })
}

/// Skips a constant expression, returning its value when it is a single
/// integer constant.
fn const_expr(r: &mut Reader<'_>) -> Result<Option<i64>> {
    let mut value = None;
    let mut ops = 0;
    loop {
        let at = r.pos();
        let op = r.byte()?;
        ops += 1;
        match op {
            0x0b => return Ok(if ops == 2 { value } else { None }),
            0x41 | 0x42 => value = Some(r.s64()?),
            0x43 => drop(r.bytes(4)?),
            0x44 => drop(r.bytes(8)?),
            // global.get, ref.func
            0x23 | 0xd2 => drop(r.u32()?),
            // ref.null
            0xd0 => drop(r.byte()?),
            // The extended-const arithmetic.
            0x6a..=0x6c | 0x7c..=0x7e => {}
            // v128.const
            0xfd => {
                if r.u32()? != 12 {
                    return Err(Error {
                        offset: at,
                        what: "unsupported constant expression",
                    });
                }
                r.bytes(16)?;
            }
            _ => {
                return Err(Error {
                    offset: at,
                    what: "unsupported constant expression",
                });
            }
        }
    }
}

pub fn global(r: &mut Reader<'_>) -> Result<Global> {
    Ok(Global {
        ty: globaltype(r)?,
        init: const_expr(r)?,
    })
}

fn kind(r: &mut Reader<'_>) -> Result<Kind> {
    Ok(match r.byte()? {
        0 => Kind::Func,
        1 => Kind::Table,
        2 => Kind::Memory,
        3 => Kind::Global,
        4 => Kind::Tag,
        _ => {
            return Err(Error {
                offset: r.pos() - 1,
                what: "unknown import or export kind",
            });
        }
    })
}

pub fn import<'a>(r: &mut Reader<'a>) -> Result<Import<'a>> {
    let module = r.name()?;
    let name = r.name()?;
    let ty = match kind(r)? {
        Kind::Func => ImportType::Func(r.u32()?),
        Kind::Table => ImportType::Table(table(r)?),
        Kind::Memory => ImportType::Memory(limits(r)?),
        Kind::Global => ImportType::Global(globaltype(r)?),
        Kind::Tag => {
            r.byte()?;
            ImportType::Tag(r.u32()?)
        }
    };
    Ok(Import { module, name, ty })
}

pub fn export<'a>(r: &mut Reader<'a>) -> Result<Export<'a>> {
    Ok(Export {
        name: r.name()?,
        kind: kind(r)?,
        index: r.u32()?,
    })
}
