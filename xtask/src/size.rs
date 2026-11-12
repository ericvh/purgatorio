//! `cargo xtask size`: where the bytes are. Prints the kernel's section
//! sizes, its code by crate, and the guests' sizes, so growth shows up in
//! a diff of two reports. It reads the ELF and the modules itself; no
//! other tool is needed.
//!
//! Code is attributed from the symbol table: each function's size goes to
//! the first crate its mangled name mentions. For generic code that is
//! the crate that defines the function or the trait impl, not the one
//! that instantiated it, and identical functions the linker folded
//! together go to whichever name comes first, so the split is
//! approximate.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::Result;

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

pub(crate) fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

pub(crate) fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// A NUL-terminated string at `o`.
pub(crate) fn cstr(b: &[u8], o: usize) -> Option<&str> {
    let s = b.get(o..)?;
    let end = s.iter().position(|&c| c == 0)?;
    std::str::from_utf8(&s[..end]).ok()
}

pub(crate) struct Section {
    name: String,
    pub(crate) kind: u32,
    pub(crate) flags: u64,
    pub(crate) addr: u64,
    pub(crate) offset: usize,
    pub(crate) size: usize,
    pub(crate) link: usize,
    pub(crate) entsize: usize,
}

pub(crate) const SHT_SYMTAB: u32 = 2;
const SHT_NOBITS: u32 = 8;
const SHF_ALLOC: u64 = 2;
pub(crate) const SHF_EXECINSTR: u64 = 4;
pub(crate) const STT_FUNC: u8 = 2;

pub(crate) fn sections(elf: &[u8]) -> Option<Vec<Section>> {
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = u64_at(elf, 0x28)? as usize;
    let shentsize = u16_at(elf, 0x3a)? as usize;
    let shnum = u16_at(elf, 0x3c)? as usize;
    let shstrndx = u16_at(elf, 0x3e)? as usize;
    let header = |i: usize| shoff + i * shentsize;
    let strtab = u64_at(elf, header(shstrndx) + 0x18)? as usize;
    let mut out = Vec::new();
    for i in 0..shnum {
        let h = header(i);
        out.push(Section {
            name: cstr(elf, strtab + u32_at(elf, h)? as usize)?.to_string(),
            kind: u32_at(elf, h + 4)?,
            flags: u64_at(elf, h + 8)?,
            addr: u64_at(elf, h + 0x10)?,
            offset: u64_at(elf, h + 0x18)? as usize,
            size: u64_at(elf, h + 0x20)? as usize,
            link: u32_at(elf, h + 0x28)? as usize,
            entsize: u64_at(elf, h + 0x38)? as usize,
        });
    }
    Some(out)
}

/// The crate a Rust symbol belongs to: the first crate root in a v0
/// mangled name (`C[s<base-62>_]<len><name>`), or the first path element
/// of a legacy one (`_ZN<len><name>`). `None` for other symbols.
pub(crate) fn symbol_crate(sym: &str) -> Option<&str> {
    fn ident(s: &str) -> Option<&str> {
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        let len: usize = s.get(..digits)?.parse().ok()?;
        let rest = s.get(digits..)?;
        let rest = rest.strip_prefix('_').unwrap_or(rest);
        rest.get(..len)
    }
    if let Some(rest) = sym.strip_prefix("_ZN") {
        return ident(rest).filter(|n| !n.starts_with("_$"));
    }
    let body = sym.strip_prefix("_R")?;
    let bytes = body.as_bytes();
    for (i, _) in body.match_indices('C') {
        let mut at = i + 1;
        if bytes.get(at) == Some(&b's') {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) {
                at += 1;
            }
            if bytes.get(at) != Some(&b'_') {
                continue;
            }
            at += 1;
        }
        if bytes.get(at).is_some_and(u8::is_ascii_digit)
            && let Some(name) = ident(&body[at..])
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Some(name);
        }
    }
    None
}

/// Crates reported as one line.
fn group(krate: &str) -> &str {
    match krate {
        "core" | "alloc" | "compiler_builtins" => "core + alloc",
        "wasmi" | "wasmi_core" | "wasmi_ir" | "wasmi_collections" => "wasmi",
        "purg_kernel" | "purg_aarch64" => "purg_kernel + purg_aarch64",
        "purg_vm" | "purg_vm_wasmi" => "purg_vm + purg_vm_wasmi",
        k => k,
    }
}

fn kib(n: usize) -> String {
    format!("{:.1}", n as f64 / 1024.0)
}

/// Prints the section sizes of the ELF at `path` and its code by crate.
pub fn kernel(path: &Path) -> Result<()> {
    let elf = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bad = || format!("{}: not a 64-bit little-endian ELF", path.display());
    let secs = sections(&elf).ok_or_else(bad)?;
    println!("kernel: {}", path.display());
    println!("  {:<10} {:>10} {:>10}", "section", "bytes", "KiB");
    let (mut load, mut ram) = (0, 0);
    for s in secs.iter().filter(|s| s.flags & SHF_ALLOC != 0) {
        println!("  {:<10} {:>10} {:>10}", s.name, s.size, kib(s.size));
        ram += s.size;
        if s.kind != SHT_NOBITS {
            load += s.size;
        }
    }
    println!("  {:<10} {:>10} {:>10}", "loaded", load, kib(load));
    println!("  {:<10} {:>10} {:>10}", "in RAM", ram, kib(ram));

    let text: usize = secs
        .iter()
        .filter(|s| s.flags & SHF_EXECINSTR != 0)
        .map(|s| s.size)
        .sum();
    let Some(symtab) = secs.iter().find(|s| s.kind == SHT_SYMTAB) else {
        println!("  (no symbol table: code by crate is not available)");
        return Ok(());
    };
    let strtab = secs.get(symtab.link).ok_or_else(bad)?.offset;
    // The linker folds identical functions onto one address under
    // several names: each address counts once, under its first name.
    let mut funcs: BTreeMap<u64, (usize, &str)> = BTreeMap::new();
    for i in 0..symtab.size / symtab.entsize.max(1) {
        let e = symtab.offset + i * symtab.entsize;
        let info = *elf.get(e + 4).ok_or_else(bad)?;
        let addr = u64_at(&elf, e + 8).ok_or_else(bad)?;
        let size = u64_at(&elf, e + 0x10).ok_or_else(bad)? as usize;
        if info & 0xf != STT_FUNC || size == 0 {
            continue;
        }
        let name = u32_at(&elf, e).and_then(|n| cstr(&elf, strtab + n as usize));
        let krate = name.and_then(symbol_crate).map_or("(other)", group);
        funcs.entry(addr).or_insert((size, krate));
    }
    let mut by_crate: BTreeMap<&str, usize> = BTreeMap::new();
    let mut named = 0;
    for &(size, krate) in funcs.values() {
        *by_crate.entry(krate).or_default() += size;
        named += size;
    }
    let mut rows: Vec<(&str, usize)> = by_crate.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    println!("  code by crate ({} KiB of text):", kib(text));
    let row = |name: &str, size: usize| {
        println!(
            "  {:<28} {:>8} {:>5.1}%",
            name,
            kib(size),
            100.0 * size as f64 / text.max(1) as f64
        );
    };
    // Crates under 4 KiB are one line.
    const SMALL: usize = 4096;
    let mut rest = 0;
    for (krate, size) in rows {
        if size >= SMALL {
            row(krate, size);
        } else {
            rest += size;
        }
    }
    row("(crates under 4 KiB)", rest);
    row("(no symbol: asm, padding)", text.saturating_sub(named));
    Ok(())
}

/// What a guest module holds.
#[derive(Default)]
struct Guest {
    bytes: usize,
    code: usize,
    data: usize,
    /// Initial linear memory, in 64 KiB pages.
    pages: u64,
}

fn leb(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn guest(wasm: &[u8]) -> Option<Guest> {
    if wasm.get(..4)? != b"\0asm" {
        return None;
    }
    let mut g = Guest {
        bytes: wasm.len(),
        ..Guest::default()
    };
    let mut pos = 8;
    while pos < wasm.len() {
        let id = *wasm.get(pos)?;
        pos += 1;
        let size = leb(wasm, &mut pos)? as usize;
        match id {
            // The memory section: a count, then each memory's limits.
            5 => {
                let mut p = pos;
                if leb(wasm, &mut p)? > 0 {
                    p += 1;
                    g.pages = leb(wasm, &mut p)?;
                }
            }
            10 => g.code = size,
            11 => g.data = size,
            _ => {}
        }
        pos += size;
    }
    Some(g)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, Guest)>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let rel = format!("{rel}/{name}");
        if path.is_dir() {
            walk(&path, &rel, out)?;
        } else if name.ends_with(".wasm") {
            let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let g = guest(&bytes).ok_or_else(|| format!("{}: not a module", path.display()))?;
            out.push((rel, g));
        }
    }
    Ok(())
}

/// Prints the sizes of the modules under `root/bin`: a summary, and with
/// `each` a line per module.
pub fn guests(root: &Path, each: bool) -> Result<()> {
    let mut all = Vec::new();
    walk(&root.join("bin"), "/bin", &mut all)?;
    if all.is_empty() {
        println!("guests: none in root/bin (run `cargo xtask build-root`)");
        return Ok(());
    }
    if each {
        println!(
            "  {:<28} {:>9} {:>9} {:>9} {:>12}",
            "module", "bytes", "code", "data", "memory KiB"
        );
        for (name, g) in &all {
            println!(
                "  {:<28} {:>9} {:>9} {:>9} {:>12}",
                name,
                g.bytes,
                g.code,
                g.data,
                g.pages * 64
            );
        }
    }
    let total: usize = all.iter().map(|(_, g)| g.bytes).sum();
    let mut sizes: Vec<usize> = all.iter().map(|(_, g)| g.bytes).collect();
    sizes.sort_unstable();
    println!(
        "guests: {} modules, {} KiB; smallest {} KiB, median {} KiB, largest {} KiB",
        all.len(),
        kib(total),
        kib(sizes[0]),
        kib(sizes[sizes.len() / 2]),
        kib(sizes[sizes.len() - 1])
    );
    let mut by_pages: BTreeMap<u64, usize> = BTreeMap::new();
    for (_, g) in &all {
        *by_pages.entry(g.pages).or_default() += 1;
    }
    let dist: Vec<String> = by_pages
        .iter()
        .map(|(pages, n)| format!("{n} at {} KiB", pages * 64))
        .collect();
    println!("guests: initial linear memory: {}", dist.join(", "));
    for name in ["/bin/echo.wasm", "/bin/sh.wasm", "/bin/calc.wasm"] {
        if let Some((_, g)) = all.iter().find(|(n, _)| n == name) {
            println!(
                "  {:<16} {:>7} bytes (code {}, data {}), memory {} KiB",
                name,
                g.bytes,
                g.code,
                g.data,
                g.pages * 64
            );
        }
    }
    Ok(())
}

fn module(path: &Path) -> Result<Guest> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::wasm::check(&bytes, true).map_err(|e| format!("{}: {e}", path.display()))?;
    guest(&bytes).ok_or_else(|| format!("{}: not a module", path.display()))
}

/// The commands in `appl/small/multi-std`; the same list as its
/// `build.rs`.
const MULTI_STD: &[&str] = &[
    "basename",
    "cat",
    "cleanname",
    "cmp",
    "cp",
    "date",
    "echo",
    "ls",
    "mkdir",
    "mv",
    "pwd",
    "rm",
    "sleep",
    "tee",
    "touch",
    "wc",
];

/// Compares the experimental guests built from `appl/small` (in
/// `no_std_dir` and `std_dir`) with the `std` commands in `root/bin`.
pub fn small(root: &Path, no_std_dir: &Path, std_dir: &Path) -> Result<()> {
    let bin = root.join("bin");
    println!(
        "  {:<10} {:>10} {:>12} {:>10} {:>12}",
        "command", "std bytes", "memory KiB", "no_std", "memory KiB"
    );
    for name in ["echo", "cat", "sleep"] {
        let std = module(&bin.join(format!("{name}.wasm")))?;
        let small = module(&no_std_dir.join(format!("{name}.wasm")))?;
        println!(
            "  {:<10} {:>10} {:>12} {:>10} {:>12}",
            name,
            std.bytes,
            std.pages * 64,
            small.bytes,
            small.pages * 64
        );
    }
    let multi = module(&no_std_dir.join("multi.wasm"))?;
    println!(
        "  no_std multi-call (echo, cat, sleep in one module): {} bytes, memory {} KiB",
        multi.bytes,
        multi.pages * 64
    );
    let mut separate = 0;
    for name in MULTI_STD {
        separate += module(&bin.join(format!("{name}.wasm")))?.bytes;
    }
    let multi = module(&std_dir.join("multi-std.wasm"))?;
    println!(
        "  std multi-call ({} commands in one module): {} bytes, memory {} KiB; \
         the same commands as separate modules: {} bytes ({:.1}x)",
        MULTI_STD.len(),
        multi.bytes,
        multi.pages * 64,
        separate,
        separate as f64 / multi.bytes.max(1) as f64
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crates_from_symbols() {
        assert_eq!(
            symbol_crate("_RNvNtCs1ihk1OVxK6B_4sha26sha51211compress512"),
            Some("sha2")
        );
        assert_eq!(
            symbol_crate("_RNCNvNtCs7d0xNX47yyg_11purg_kernel6system5start0B5_"),
            Some("purg_kernel")
        );
        assert_eq!(
            symbol_crate(
                "_RINvXsQ_NtCs5dwrRVMhJnp_8wasmi_ir6encodeNtNtB8_2op2OpNtB6_6Encode6encodeNtCsgYvR3YrwZHH_5wasmi6EngineE"
            ),
            Some("wasmi_ir")
        );
        assert_eq!(
            symbol_crate("_ZN4core3fmt5write17h0123456789abcdefE"),
            Some("core")
        );
        assert_eq!(symbol_crate("purg_aarch64_switch"), None);
    }

    #[test]
    fn module_sections() {
        // (module (memory 17) (func) (data (i32.const 0) "ab"))
        let wasm = [
            0, b'a', b's', b'm', 1, 0, 0, 0, // header
            1, 4, 1, 0x60, 0, 0, // type
            3, 2, 1, 0, // function
            5, 3, 1, 0, 17, // memory
            10, 4, 1, 2, 0, 0x0b, // code
            11, 8, 1, 0, 0x41, 0, 0x0b, 2, b'a', b'b', // data
        ];
        let g = guest(&wasm).unwrap();
        assert_eq!((g.bytes, g.code, g.data, g.pages), (wasm.len(), 4, 8, 17));
        assert!(guest(b"not wasm").is_none());
    }
}
