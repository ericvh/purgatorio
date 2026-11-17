use alloc::vec::Vec;

use purg_abi::{ABI_SECTION, ABI_VERSION, error, mode};
use purg_core::{Error, Prog};

/// Reads a module through the prog's namespace and checks it is a WASM
/// binary whose `purgatorio.abi` section, if present, matches this host.
/// Modules without the section (third-party WASI programs) are accepted.
/// The module is then checked against the prog's module policy and
/// recorded in the prog with its digest.
pub fn load_module(prog: &mut Prog, path: &str) -> Result<Vec<u8>, Error> {
    let fd = prog.open(path, mode::OREAD)?;
    let mut wasm = Vec::new();
    let mut buf = [0u8; 8192];
    let res = loop {
        match prog.read(fd, &mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => wasm.extend_from_slice(&buf[..n]),
            Err(e) => break Err(e),
        }
    };
    prog.close(fd)?;
    res?;
    match abi_version(&wasm) {
        Err(Malformed) => Err(Error::new(error::EBADEXEC)),
        Ok(Some(v)) if v != ABI_VERSION.as_bytes() => Err(Error::from(alloc::format!(
            "{}: abi version {} not supported",
            error::EBADEXEC,
            alloc::string::String::from_utf8_lossy(v)
        ))),
        Ok(_) => Ok(()),
    }?;
    // The prog's module policy (`docs/signing.md`): the digest is taken
    // from these bytes, which are the ones the engine then runs.
    prog.check_module(path, &wasm)?;
    Ok(wasm)
}

/// A byte string that is not a well-formed WASM module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

fn leb_u32(buf: &[u8], pos: &mut usize) -> Result<u32, Malformed> {
    let mut v: u32 = 0;
    for shift in (0..35).step_by(7) {
        let b = *buf.get(*pos).ok_or(Malformed)?;
        *pos += 1;
        v |= u32::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Malformed)
}

/// Returns the contents of the `purgatorio.abi` custom section, `Ok(None)`
/// if there is none, or [`Malformed`] if `wasm` is not a well-formed module header
/// and section list.
pub fn abi_version(wasm: &[u8]) -> Result<Option<&[u8]>, Malformed> {
    if wasm.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return Err(Malformed);
    }
    let mut pos = 8;
    while pos < wasm.len() {
        let id = wasm[pos];
        pos += 1;
        let size = leb_u32(wasm, &mut pos)? as usize;
        let end = pos
            .checked_add(size)
            .filter(|&e| e <= wasm.len())
            .ok_or(Malformed)?;
        if id == 0 {
            let mut p = pos;
            let nlen = leb_u32(wasm, &mut p)? as usize;
            let name = wasm.get(p..p + nlen).ok_or(Malformed)?;
            if name == ABI_SECTION.as_bytes() {
                return Ok(Some(wasm.get(p + nlen..end).ok_or(Malformed)?));
            }
        }
        pos = end;
    }
    Ok(None)
}

/// The flags byte of the limits of `wasm`'s memory, defined or imported:
/// bit 1 is `shared` and bit 2 is `i64`. `None` for a module with no
/// memory, or one that is not well formed.
fn memory_flags(wasm: &[u8]) -> Option<u8> {
    let scan = || -> Result<Option<u8>, Malformed> {
        if wasm.get(..8) != Some(b"\0asm\x01\0\0\0") {
            return Err(Malformed);
        }
        let mut pos = 8;
        while pos < wasm.len() {
            let id = wasm[pos];
            pos += 1;
            let size = leb_u32(wasm, &mut pos)? as usize;
            let end = pos
                .checked_add(size)
                .filter(|&e| e <= wasm.len())
                .ok_or(Malformed)?;
            let body = &wasm[pos..end];
            let mut p = 0;
            match id {
                // The memory section: a count, then each memory's limits.
                5 => {
                    if leb_u32(body, &mut p)? > 0 {
                        return Ok(Some(*body.get(p).ok_or(Malformed)?));
                    }
                }
                // The import section: an imported memory comes first.
                2 => {
                    for _ in 0..leb_u32(body, &mut p)? {
                        for _ in 0..2 {
                            let n = leb_u32(body, &mut p)? as usize;
                            p = p.checked_add(n).ok_or(Malformed)?;
                        }
                        let kind = *body.get(p).ok_or(Malformed)?;
                        p += 1;
                        match kind {
                            // A function: its type index.
                            0 => drop(leb_u32(body, &mut p)?),
                            // A memory.
                            2 => return Ok(Some(*body.get(p).ok_or(Malformed)?)),
                            // A table, global or tag: none in a guest
                            // this host runs, and nothing after it is read.
                            _ => return Ok(None),
                        }
                    }
                }
                _ => {}
            }
            pos = end;
        }
        Ok(None)
    };
    scan().unwrap_or(None)
}

/// Whether `wasm` is a wasm64 module: one whose memory, defined or
/// imported, is indexed by `i64` (`docs/wasm64.md`). A module that is not
/// well formed is not.
pub fn memory64(wasm: &[u8]) -> bool {
    memory_flags(wasm).is_some_and(|f| f & 0x04 != 0)
}

/// Whether `wasm` is a threaded module: one whose memory is shared, so
/// that several instances, one a thread, can run on it
/// (`docs/threads.md`).
pub fn shared_memory(wasm: &[u8]) -> bool {
    memory_flags(wasm).is_some_and(|f| f & 0x02 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_with_section(name: &str, body: &[u8]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        m.push(0);
        m.push((1 + name.len() + body.len()) as u8);
        m.push(name.len() as u8);
        m.extend_from_slice(name.as_bytes());
        m.extend_from_slice(body);
        m
    }

    #[test]
    fn finds_abi_section() {
        let m = module_with_section("purgatorio.abi", b"0.1");
        assert_eq!(abi_version(&m), Ok(Some(&b"0.1"[..])));
        let m = module_with_section("name", b"x");
        assert_eq!(abi_version(&m), Ok(None));
        assert_eq!(abi_version(b"\0asm\x01\0\0\0"), Ok(None));
    }

    fn module(sections: &[(u8, &[u8])]) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        for (id, body) in sections {
            m.push(*id);
            m.push(body.len() as u8);
            m.extend_from_slice(body);
        }
        m
    }

    #[test]
    fn tells_the_widths_apart() {
        // One memory: flags, minimum.
        assert!(!memory64(&module(&[(5, &[1, 0x00, 1])])));
        assert!(memory64(&module(&[(5, &[1, 0x04, 1])])));
        assert!(memory64(&module(&[(5, &[1, 0x05, 1, 2])])));
        // An imported function `a.b` of type 0, then memory `a.m`.
        let import = [2, 1, b'a', 1, b'b', 0, 0, 1, b'a', 1, b'm', 2, 0x04, 1];
        assert!(memory64(&module(&[(2, &import)])));
        assert!(!memory64(&module(&[])));
        assert!(!memory64(b"#!/bin/sh"));
        assert!(!memory64(&module(&[(5, &[1])])));
        // Shared, with a maximum, as a threaded module imports it.
        let shared = [
            1, 3, b'e', b'n', b'v', 6, b'm', b'e', b'm', b'o', b'r', b'y', 2, 0x03, 17, 64,
        ];
        assert!(shared_memory(&module(&[(2, &shared)])));
        assert!(!memory64(&module(&[(2, &shared)])));
        assert!(!shared_memory(&module(&[(5, &[1, 0x04, 1])])));
        assert!(!shared_memory(&module(&[])));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(abi_version(b"#!/bin/sh"), Err(Malformed));
        let mut m = module_with_section("purgatorio.abi", b"0.1");
        m.truncate(m.len() - 1);
        assert_eq!(abi_version(&m), Err(Malformed));
    }
}
