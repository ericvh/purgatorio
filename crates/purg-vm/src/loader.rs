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

    #[test]
    fn rejects_garbage() {
        assert_eq!(abi_version(b"#!/bin/sh"), Err(Malformed));
        let mut m = module_with_section("purgatorio.abi", b"0.1");
        m.truncate(m.len() - 1);
        assert_eq!(abi_version(&m), Err(Malformed));
    }
}
