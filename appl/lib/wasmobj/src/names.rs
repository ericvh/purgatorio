//! Function names: from a module's `name` custom section, or from the
//! `.names` file `cargo xtask build-root` writes beside a stripped module.

use alloc::string::String;
use alloc::vec::Vec;

use crate::{Reader, Result};

/// Function names by function index, in index order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    list: Vec<(u32, String)>,
}

impl Names {
    /// Reads the function names from the contents of a `name` section.
    /// The module name and the names of locals and other items are
    /// skipped.
    pub fn from_section(data: &[u8]) -> Result<Names> {
        let mut r = Reader::new(data, 0);
        let mut list = Vec::new();
        while !r.done() {
            let id = r.byte()?;
            let size = r.u32()? as usize;
            let at = r.pos();
            let sub = r.bytes(size)?;
            if id != 1 {
                continue;
            }
            let mut s = Reader::new(sub, 0);
            list = s
                .vec(|s| Ok((s.u32()?, String::from(s.name()?))))
                .map_err(|e| crate::Error {
                    offset: at + e.offset,
                    what: e.what,
                })?;
        }
        list.sort_by_key(|e| e.0);
        Ok(Names { list })
    }

    /// Reads a `.names` file: a line per function, its index in decimal,
    /// a space, and the name, which runs to the end of the line. Lines
    /// that do not parse are skipped.
    pub fn from_text(text: &str) -> Names {
        let mut list: Vec<(u32, String)> = text
            .lines()
            .filter_map(|l| {
                let (index, name) = l.split_once(' ')?;
                Some((index.parse().ok()?, String::from(name)))
            })
            .collect();
        list.sort_by_key(|e| e.0);
        Names { list }
    }

    /// The `.names` file for these names.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (index, name) in &self.list {
            // A name is one line here whatever the module says.
            let name: String = name
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            out.push_str(&alloc::format!("{index} {name}\n"));
        }
        out
    }

    pub fn get(&self, index: u32) -> Option<&str> {
        let i = self.list.binary_search_by_key(&index, |e| e.0).ok()?;
        Some(&self.list[i].1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &str)> {
        self.list.iter().map(|(i, n)| (*i, n.as_str()))
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Replaces every name with `f` of it.
    pub fn map(&mut self, f: impl Fn(&str) -> String) {
        for e in &mut self.list {
            e.1 = f(&e.1);
        }
    }
}

/// Where the names of the module at `path` are kept: `/bin/cat.wasm` has
/// `/bin/cat.names`.
pub fn names_path(path: &str) -> String {
    let stem = path.strip_suffix(".wasm").unwrap_or(path);
    alloc::format!("{stem}.names")
}
