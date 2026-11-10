//! A reader for WebAssembly binary modules: the section list, and the
//! contents of every section except function bodies, which are only
//! measured. It never panics on malformed input: every read is checked and
//! reports the offset it failed at.
//!
//! It covers the module format the guests here use: the MVP with
//! multi-value, reference types, bulk memory, multi-memory, 64-bit
//! limits and tail calls. Types from the GC proposal are refused.
#![no_std]

extern crate alloc;

mod names;
mod read;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub use names::{Names, names_path};
pub use read::Reader;

/// The 8 bytes every module starts with.
pub const HEADER: &[u8; 8] = b"\0asm\x01\0\0\0";

/// Why a module could not be read, and the file offset of the fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub offset: usize,
    pub what: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at offset {}", self.what, self.offset)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// One section as it lies in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section<'a> {
    pub id: u8,
    /// The offset of the id byte.
    pub offset: usize,
    /// The offset of `data`.
    pub start: usize,
    /// The contents: for a custom section, what follows the name.
    pub data: &'a [u8],
    /// The size the header gives: for a custom section it counts the name.
    pub size: usize,
    /// A custom section's name.
    pub name: Option<&'a str>,
}

impl Section<'_> {
    /// The section's standard name.
    pub fn kind(&self) -> &'static str {
        match self.id {
            0 => "custom",
            1 => "type",
            2 => "import",
            3 => "function",
            4 => "table",
            5 => "memory",
            6 => "global",
            7 => "export",
            8 => "start",
            9 => "element",
            10 => "code",
            11 => "data",
            12 => "datacount",
            13 => "tag",
            _ => "unknown",
        }
    }
}

/// Splits a module into its sections. The header is checked, and each
/// section must end inside the file.
pub fn sections(wasm: &[u8]) -> Result<Vec<Section<'_>>> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return Err(Error {
            offset: 0,
            what: "not a wasm module",
        });
    }
    if wasm[4..8] != HEADER[4..] {
        return Err(Error {
            offset: 4,
            what: "unknown wasm version",
        });
    }
    let mut r = Reader::new(wasm, 8);
    let mut out = Vec::new();
    while !r.done() {
        let offset = r.pos();
        let id = r.byte()?;
        let size = r.u32()? as usize;
        let start = r.pos();
        let body = r.bytes(size)?;
        let (name, data, start) = if id == 0 {
            let mut c = Reader::new(body, 0);
            let name = c.name().map_err(|e| Error {
                offset: start + e.offset,
                what: e.what,
            })?;
            (Some(name), &body[c.pos()..], start + c.pos())
        } else {
            (None, body, start)
        };
        out.push(Section {
            id,
            offset,
            start,
            data,
            size,
            name,
        });
    }
    Ok(out)
}

/// A value type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    V128,
    FuncRef,
    ExternRef,
}

impl ValType {
    pub fn as_str(self) -> &'static str {
        match self {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F32 => "f32",
            ValType::F64 => "f64",
            ValType::V128 => "v128",
            ValType::FuncRef => "funcref",
            ValType::ExternRef => "externref",
        }
    }
}

/// A function type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

impl fmt::Display for FuncType {
    /// `(i32 i32) -> i32`; a function with no result is `(i32)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, v: &[ValType]| {
            for (i, t) in v.iter().enumerate() {
                if i > 0 {
                    f.write_str(" ")?;
                }
                f.write_str(t.as_str())?;
            }
            Ok(())
        };
        f.write_str("(")?;
        list(f, &self.params)?;
        f.write_str(")")?;
        match self.results.as_slice() {
            [] => Ok(()),
            [one] => write!(f, " -> {}", one.as_str()),
            many => {
                f.write_str(" -> (")?;
                list(f, many)?;
                f.write_str(")")
            }
        }
    }
}

/// The limits of a memory (in 64 KiB pages) or a table (in elements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min: u64,
    pub max: Option<u64>,
    pub shared: bool,
    /// Indexed by i64 (the memory64 proposal).
    pub wide: bool,
}

impl fmt::Display for Limits {
    /// `min 17 max 256`, with `max -` when there is no maximum.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "min {} max ", self.min)?;
        match self.max {
            Some(m) => write!(f, "{m}")?,
            None => f.write_str("-")?,
        }
        if self.shared {
            f.write_str(" shared")?;
        }
        if self.wide {
            f.write_str(" i64")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    pub elem: ValType,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobalType {
    pub val: ValType,
    pub mutable: bool,
}

impl fmt::Display for GlobalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mutable {
            write!(f, "mut {}", self.val.as_str())
        } else {
            f.write_str(self.val.as_str())
        }
    }
}

/// What an import or an export is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Func,
    Table,
    Memory,
    Global,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Func => "func",
            Kind::Table => "table",
            Kind::Memory => "memory",
            Kind::Global => "global",
            Kind::Tag => "tag",
        }
    }
}

/// The type of an imported item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportType {
    /// A function, with the index of its type.
    Func(u32),
    Table(Table),
    Memory(Limits),
    Global(GlobalType),
    Tag(u32),
}

impl ImportType {
    pub fn kind(&self) -> Kind {
        match self {
            ImportType::Func(_) => Kind::Func,
            ImportType::Table(_) => Kind::Table,
            ImportType::Memory(_) => Kind::Memory,
            ImportType::Global(_) => Kind::Global,
            ImportType::Tag(_) => Kind::Tag,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Import<'a> {
    pub module: &'a str,
    pub name: &'a str,
    pub ty: ImportType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Export<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub index: u32,
}

/// A global the module defines. `init` is the value when the initializer
/// is one integer constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Global {
    pub ty: GlobalType,
    pub init: Option<i64>,
}

/// A parsed module. Imported functions, tables, memories and globals come
/// first in each index space, in import order, then the module's own.
#[derive(Debug, Clone, Default)]
pub struct Module<'a> {
    pub sections: Vec<Section<'a>>,
    pub types: Vec<FuncType>,
    pub imports: Vec<Import<'a>>,
    /// The type index of each function the module defines.
    pub funcs: Vec<u32>,
    pub tables: Vec<Table>,
    pub memories: Vec<Limits>,
    pub globals: Vec<Global>,
    pub exports: Vec<Export<'a>>,
    pub start: Option<u32>,
    /// The number of element segments.
    pub elements: u32,
    /// The number of data segments.
    pub data: u32,
    /// The size in bytes of each function body, in function order.
    pub bodies: Vec<u32>,
}

impl<'a> Module<'a> {
    /// The contents of the first custom section called `name`.
    pub fn custom(&self, name: &str) -> Option<&'a [u8]> {
        self.customs(name).next()
    }

    /// The contents of every custom section called `name`.
    pub fn customs<'s>(&'s self, name: &'s str) -> impl Iterator<Item = &'a [u8]> + 's {
        self.sections
            .iter()
            .filter(move |s| s.id == 0 && s.name == Some(name))
            .map(|s| s.data)
    }

    /// How many items of `kind` the module imports: the index of its
    /// first own item of that kind.
    pub fn imported(&self, kind: Kind) -> u32 {
        self.imports.iter().filter(|i| i.ty.kind() == kind).count() as u32
    }

    /// The type of function `index`, imported or defined.
    pub fn func_type(&self, index: u32) -> Option<&FuncType> {
        let mut funcs = self.imports.iter().filter_map(|i| match i.ty {
            ImportType::Func(t) => Some(t),
            _ => None,
        });
        let nimp = self.imported(Kind::Func);
        let ty = if index < nimp {
            funcs.nth(index as usize)?
        } else {
            *self.funcs.get((index - nimp) as usize)?
        };
        self.types.get(ty as usize)
    }

    /// The function names in the module's own `name` section, if it has
    /// one that parses.
    pub fn names(&self) -> Option<Names> {
        Names::from_section(self.custom("name")?).ok()
    }
}

/// Parses a whole module.
pub fn parse(wasm: &[u8]) -> Result<Module<'_>> {
    let mut m = Module {
        sections: sections(wasm)?,
        ..Module::default()
    };
    for i in 0..m.sections.len() {
        let s = m.sections[i];
        let mut r = Reader::new(s.data, 0);
        let res = match s.id {
            1 => r.vec(read::functype).map(|v| m.types = v),
            2 => r.vec(read::import).map(|v| m.imports = v),
            3 => r.vec(Reader::u32).map(|v| m.funcs = v),
            4 => r.vec(read::table).map(|v| m.tables = v),
            5 => r.vec(read::limits).map(|v| m.memories = v),
            6 => r.vec(read::global).map(|v| m.globals = v),
            7 => r.vec(read::export).map(|v| m.exports = v),
            8 => r.u32().map(|f| m.start = Some(f)),
            9 => r.u32().map(|n| m.elements = n),
            10 => r
                .vec(|r| {
                    let n = r.u32()?;
                    r.bytes(n as usize)?;
                    Ok(n)
                })
                .map(|v| m.bodies = v),
            // The data count section says the same as the data section.
            11 | 12 => r.u32().map(|n| m.data = n),
            _ => Ok(()),
        };
        // Sections read whole must be used up; element and data segments
        // are only counted.
        let whole = matches!(s.id, 1..=8 | 10 | 12);
        match res {
            Err(e) => {
                return Err(Error {
                    offset: s.start + e.offset,
                    what: e.what,
                });
            }
            Ok(()) if whole && !r.done() => {
                return Err(Error {
                    offset: s.start + r.pos(),
                    what: "junk after section",
                });
            }
            Ok(()) => {}
        }
    }
    Ok(m)
}

/// The path-like strings ending in `.wasm` in the module's data segments:
/// the modules a command may `load` or start. Rust packs string constants
/// together, so a name is taken from the first `/` of the run of path
/// characters it ends.
pub fn module_paths(m: &Module<'_>) -> Vec<String> {
    fn path_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-' | b'+')
    }
    const SUFFIX: &[u8] = b".wasm";
    let mut out: Vec<String> = Vec::new();
    for s in m.sections.iter().filter(|s| s.id == 11) {
        let d = s.data;
        let mut i = 0;
        while let Some(at) = d[i..].windows(SUFFIX.len()).position(|w| w == SUFFIX) {
            let end = i + at + SUFFIX.len();
            i = end;
            let mut start = end - SUFFIX.len();
            while start > 0 && path_byte(d[start - 1]) {
                start -= 1;
            }
            let run = &d[start..end];
            let Some(slash) = run.iter().position(|&b| b == b'/') else {
                continue;
            };
            let Ok(path) = core::str::from_utf8(&run[slash..]) else {
                continue;
            };
            let base = path.rsplit('/').next().unwrap_or("");
            // `x.wasm.wasm` is two strings that happen to be neighbours.
            let twice = path[..path.len() - SUFFIX.len()].contains(".wasm");
            if base.len() > SUFFIX.len() && !twice && !out.iter().any(|p| p == path) {
                out.push(String::from(path));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests;
