//! Guest module checks: every import must be one the host implements, and
//! Purgatorio-built modules must carry a matching `purgatorio.abi` section.

use std::collections::BTreeSet;

fn host_imports() -> BTreeSet<(&'static str, &'static str)> {
    let mut set = BTreeSet::new();
    macro_rules! collect {
        ($module:expr, $name:literal, $f:path, ($($a:ident : $t:ty),*) -> $r:ty) => {
            set.insert(($module, $name));
        };
    }
    purg_vm::for_each_import!(collect);
    // `load` and `call` are registered by each engine directly: they need
    // that engine's store, which the shared host functions do not have.
    set.insert((purg_abi::MODULE, purg_abi::sys::LOAD));
    set.insert((purg_abi::MODULE, purg_abi::sys::CALL));
    set
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.buf.get(self.pos).ok_or("truncated module")?;
        self.pos += 1;
        Ok(b)
    }

    fn leb(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad LEB128".into())
    }

    fn name(&mut self) -> Result<&'a str, String> {
        let n = self.leb()? as usize;
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let s = &self.buf[self.pos..end.ok_or("truncated name")?];
        self.pos += n;
        std::str::from_utf8(s).map_err(|_| "non-UTF-8 name".into())
    }

    fn limits(&mut self) -> Result<(), String> {
        let flags = self.byte()?;
        self.leb()?;
        if flags & 1 != 0 {
            self.leb()?;
        }
        Ok(())
    }
}

/// `(module, name)` of every import in the module.
fn imports(wasm: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut r = Reader { buf: wasm, pos: 8 };
    let mut out = Vec::new();
    while r.pos < wasm.len() {
        let id = r.byte()?;
        let size = r.leb()? as usize;
        let end = r.pos + size;
        if id == 2 {
            for _ in 0..r.leb()? {
                let module = r.name()?.to_string();
                let name = r.name()?.to_string();
                match r.byte()? {
                    0 => {
                        r.leb()?;
                    }
                    1 => {
                        r.byte()?;
                        r.limits()?;
                    }
                    2 => r.limits()?,
                    3 => {
                        r.byte()?;
                        r.byte()?;
                    }
                    4 => {
                        r.byte()?;
                        r.leb()?;
                    }
                    k => return Err(format!("unknown import kind {k}")),
                }
                out.push((module, name));
            }
        }
        r.pos = end;
    }
    Ok(out)
}

pub fn check(wasm: &[u8], require_abi: bool) -> Result<(), String> {
    match purg_vm::abi_version(wasm) {
        Err(purg_vm::Malformed) => return Err("not a WASM module".into()),
        Ok(None) if require_abi => {
            return Err(format!("missing {} custom section", purg_abi::ABI_SECTION));
        }
        Ok(Some(v)) if v != purg_abi::ABI_VERSION.as_bytes() => {
            return Err(format!("ABI version {:?}", String::from_utf8_lossy(v)));
        }
        Ok(_) => {}
    }
    let host = host_imports();
    let missing: Vec<String> = imports(wasm)?
        .into_iter()
        .filter(|(m, n)| !host.contains(&(m.as_str(), n.as_str())))
        .map(|(m, n)| format!("{m}.{n}"))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "imports not implemented by the host: {}",
            missing.join(", ")
        ))
    }
}

/// Custom sections the linker writes that the installed module does not
/// carry. `strip = "symbols"` would leave them out too, but then the
/// function names would be lost.
const STRIPPED: &[&str] = &["name", "producers", "target_features"];

/// Splits a linked guest into the module to install and its function
/// names: `(module, names)`, where `module` is `wasm` without the
/// [`STRIPPED`] sections and `names` is the text of its `.names` file
/// (`wasmobj::Names`): one `index name` line per function, demangled.
///
/// The names stay out of the module because the `name` section also
/// holds the output file's name, which carries a Cargo hash of the build
/// host. The demangled function names carry no hash.
pub fn split(wasm: &[u8]) -> Result<(Vec<u8>, String), String> {
    let sections = wasmobj::sections(wasm).map_err(|e| e.to_string())?;
    let mut module = wasmobj::HEADER.to_vec();
    let mut names = wasmobj::Names::default();
    for (i, s) in sections.iter().enumerate() {
        if s.id == 0 && s.name == Some("name") {
            names =
                wasmobj::Names::from_section(s.data).map_err(|e| format!("name section: {e}"))?;
        }
        if s.id == 0 && s.name.is_some_and(|n| STRIPPED.contains(&n)) {
            continue;
        }
        let end = sections.get(i + 1).map_or(wasm.len(), |n| n.offset);
        module.extend_from_slice(&wasm[s.offset..end]);
    }
    // `{:#}` leaves out the hash a legacy name ends with and the crate
    // disambiguators of a v0 name.
    names.map(|n| format!("{:#}", rustc_demangle::demangle(n)));
    Ok((module, names.to_text()))
}
