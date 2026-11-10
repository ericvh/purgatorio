//! A reader for the POSIX ustar archives the kernel boots from: `cargo xtask
//! initrd` packs `root/bin` and `root/lib` into one, QEMU loads it with
//! `-initrd`, and [`crate::devram::RamTree::add_tar`] seeds the root tree from
//! it.
//!
//! Only directories (`5`) and regular files (`0`, NUL, `7`) are kept; links,
//! devices and FIFOs are skipped. Extended headers (pax `x`/`g`, GNU `L`/`K`)
//! would change the meaning of the entry after them, so they are errors.
//! Bad checksums, bad octal fields, sizes past the end of the archive,
//! absolute paths and `..` components are errors too. Parsing never panics
//! and reads the archive bytewise, so it needs no alignment.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};

/// The size of a header and the unit file data is padded to.
pub const BLOCK: usize = 512;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
}

/// One directory or regular file in an archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// The path, `/`-separated, without a leading `/`, `.` or empty
    /// components. Never empty and never containing `..`.
    pub path: String,
    pub kind: Kind,
    /// Permission bits (`mode & 0o777`).
    pub perm: u32,
    /// The file's contents, borrowed from the archive; empty for a directory.
    pub data: &'a [u8],
}

fn bad(off: usize, what: &str) -> Error {
    Error::from(format!("initrd: {what} in header at offset {off}"))
}

/// The bytes of a NUL-terminated field.
fn field(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    }
}

/// An octal number field: optional leading spaces, digits, then NUL or
/// spaces. An empty field is zero.
fn octal(b: &[u8]) -> Option<u64> {
    let b = field(b);
    let start = b.iter().position(|&c| c != b' ').unwrap_or(b.len());
    let b = &b[start..];
    let end = b.iter().position(|&c| c == b' ').unwrap_or(b.len());
    if b[end..].iter().any(|&c| c != b' ') {
        return None;
    }
    let mut v: u64 = 0;
    for &c in &b[..end] {
        if !(b'0'..=b'7').contains(&c) {
            return None;
        }
        v = v.checked_mul(8)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(v)
}

/// Joins prefix and name and normalizes the result.
fn entry_path(off: usize, prefix: &[u8], name: &[u8]) -> Result<String> {
    let prefix = core::str::from_utf8(field(prefix)).map_err(|_| bad(off, "non-UTF-8 path"))?;
    let name = core::str::from_utf8(field(name)).map_err(|_| bad(off, "non-UTF-8 path"))?;
    let first = if prefix.is_empty() { name } else { prefix };
    if first.starts_with('/') {
        return Err(bad(off, "absolute path"));
    }
    let mut out = String::new();
    for e in prefix.split('/').chain(name.split('/')) {
        match e {
            "" | "." => {}
            ".." => return Err(bad(off, "`..` in path")),
            e => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(e);
            }
        }
    }
    Ok(out)
}

fn checksum_ok(h: &[u8]) -> bool {
    let Some(want) = octal(&h[148..156]) else {
        return false;
    };
    let sum: u64 = h
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(c)
            }
        })
        .sum();
    sum == want
}

/// Parses an archive into its directories and regular files, in archive
/// order. A later entry with the same path replaces an earlier one, as with
/// `tar x`. The whole archive is checked before anything is returned, including
/// that no file is also used as a directory.
pub fn entries(archive: &[u8]) -> Result<Vec<Entry<'_>>> {
    let mut out: Vec<Entry<'_>> = Vec::new();
    // Every path seen, with its kind; parents count as directories.
    let mut kinds: BTreeMap<String, (Kind, usize)> = BTreeMap::new();
    let mut off = 0;
    while off < archive.len() {
        let Some(h) = archive.get(off..off + BLOCK) else {
            return Err(bad(off, "truncated header"));
        };
        if h.iter().all(|&c| c == 0) {
            // End of archive. Anything after the zero blocks is padding.
            break;
        }
        if &h[257..262] != b"ustar" {
            return Err(bad(off, "bad magic"));
        }
        if !checksum_ok(h) {
            return Err(bad(off, "bad checksum"));
        }
        let size = octal(&h[124..136]).ok_or_else(|| bad(off, "bad size"))?;
        let mode = octal(&h[100..108]).ok_or_else(|| bad(off, "bad mode"))?;
        let data_start = off + BLOCK;
        let size = usize::try_from(size).map_err(|_| bad(off, "bad size"))?;
        let data_end = data_start
            .checked_add(size)
            .filter(|&e| e <= archive.len())
            .ok_or_else(|| bad(off, "size past end of archive"))?;
        let next = data_end
            .checked_next_multiple_of(BLOCK)
            .ok_or_else(|| bad(off, "bad size"))?;
        let kind = match h[156] {
            b'0' | 0 | b'7' => Some(Kind::File),
            b'5' => Some(Kind::Dir),
            b'x' | b'g' | b'L' | b'K' => return Err(bad(off, "unsupported extended header")),
            _ => None,
        };
        if let Some(kind) = kind {
            let path = entry_path(off, &h[345..500], &h[0..100])?;
            if path.is_empty() {
                if kind == Kind::File {
                    return Err(bad(off, "empty file name"));
                }
                // `./`: the root itself.
                off = next;
                continue;
            }
            let mut parent = path.as_str();
            while let Some((p, _)) = parent.rsplit_once('/') {
                parent = p;
                match kinds.get(parent) {
                    Some(&(Kind::File, _)) => return Err(bad(off, "file used as a directory")),
                    Some(_) => break,
                    None => {
                        kinds.insert(String::from(parent), (Kind::Dir, usize::MAX));
                    }
                }
            }
            let data = if kind == Kind::File {
                &archive[data_start..data_end]
            } else {
                &[]
            };
            let e = Entry {
                path,
                kind,
                perm: (mode & 0o777) as u32,
                data,
            };
            match kinds.get(&e.path) {
                Some(&(k, _)) if k != kind => return Err(bad(off, "file and directory clash")),
                Some(&(_, i)) if i != usize::MAX => out[i] = e,
                _ => {
                    kinds.insert(e.path.clone(), (kind, out.len()));
                    out.push(e);
                }
            }
        }
        off = next;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A ustar header block, like the xtask writer's.
    fn header(name: &str, prefix: &str, typ: u8, mode: u32, size: usize) -> Vec<u8> {
        let mut h = vec![0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = typ;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        fix_sum(&mut h);
        h
    }

    fn fix_sum(h: &mut [u8]) {
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&c| u32::from(c)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }

    fn file(name: &str, data: &[u8]) -> Vec<u8> {
        let mut v = header(name, "", b'0', 0o644, data.len());
        v.extend_from_slice(data);
        v.resize(v.len().next_multiple_of(BLOCK), 0);
        v
    }

    fn end(mut v: Vec<u8>) -> Vec<u8> {
        v.resize(v.len() + 2 * BLOCK, 0);
        v
    }

    #[test]
    fn dirs_files_and_prefix() {
        let mut a = header("bin/", "", b'5', 0o755, 0);
        a.extend(file("bin/sh.wasm", b"\0asm"));
        a.extend(header("deep.txt", "lib/a/b", b'0', 0o600, 0));
        a.extend(header("sym", "", b'2', 0o777, 0));
        let a = end(a);
        let e = entries(&a).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(
            (e[0].path.as_str(), e[0].kind, e[0].perm),
            ("bin", Kind::Dir, 0o755)
        );
        assert_eq!(
            (e[1].path.as_str(), e[1].data),
            ("bin/sh.wasm", &b"\0asm"[..])
        );
        assert_eq!((e[2].path.as_str(), e[2].perm), ("lib/a/b/deep.txt", 0o600));
    }

    #[test]
    fn empty_and_unterminated() {
        assert!(entries(&[]).unwrap().is_empty());
        assert!(entries(&[0; 1024]).unwrap().is_empty());
        // No end-of-archive blocks: stop at the end of the data.
        assert_eq!(entries(&file("x", b"1")).unwrap().len(), 1);
    }

    #[test]
    fn later_entries_replace() {
        let mut a = file("x", b"old");
        a.extend(file("./x", b"new"));
        let e = entries(&a).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].data, b"new");
    }

    fn err(a: &[u8]) -> String {
        String::from(entries(a).unwrap_err().as_str())
    }

    #[test]
    fn malformed() {
        assert!(err(&[1; 100]).contains("truncated header"));
        let mut a = file("x", b"data");
        a[0] = b'y';
        assert!(err(&a).contains("bad checksum"));
        let mut a = file("x", b"data");
        a[257] = b'X';
        assert!(err(&a).contains("bad magic"));
        let mut a = header("x", "", b'0', 0, 0);
        a[124] = b'9';
        fix_sum(&mut a);
        assert!(err(&a).contains("bad size"));
        let mut a = header("x", "", b'0', 0, 0);
        a[100] = b'z';
        fix_sum(&mut a);
        assert!(err(&a).contains("bad mode"));
        let mut a = header("x", "", b'0', 0, 0);
        a[124] = 0x80; // base-256
        fix_sum(&mut a);
        assert!(err(&a).contains("bad size"));
        let a = header("x", "", b'0', 0o644, 513);
        assert!(err(&a).contains("past end"));
        let a = header("x", "", b'0', 0o644, 0o77777777777);
        assert!(err(&a).contains("past end"));
        // Data shorter than the size but in the archive: still past the end.
        let mut a = header("x", "", b'0', 0o644, 600);
        a.extend([0u8; 512]);
        assert!(err(&a).contains("past end"));
        let mut a = file("x", b"");
        a.truncate(511);
        assert!(err(&a).contains("truncated"));
    }

    #[test]
    fn bad_paths() {
        assert!(err(&file("/etc/passwd", b"")).contains("absolute"));
        assert!(err(&header("x", "/abs", b'0', 0, 0)).contains("absolute"));
        assert!(err(&file("../x", b"")).contains(".."));
        assert!(err(&file("a/../../x", b"")).contains(".."));
        assert!(err(&header("x", "a/..", b'0', 0, 0)).contains(".."));
        assert!(err(&file("./", b"")).contains("empty"));
        assert!(err(&header("x", "", b'L', 0, 0)).contains("extended"));
        assert!(err(&header("x", "", b'x', 0, 0)).contains("extended"));
        let mut a = file("f", b"");
        a.extend(file("f/g", b""));
        assert!(err(&a).contains("file used as a directory"));
        let mut a = file("f/g", b"");
        a.extend(file("f", b""));
        assert!(err(&a).contains("clash"));
        let mut a = vec![0xffu8, 0xfe];
        a.extend(&file("x", b"")[2..]);
        fix_sum(&mut a);
        assert!(err(&a).contains("UTF-8"));
    }

    #[test]
    fn root_dir_entry_is_skipped() {
        let a = end(header("./", "", b'5', 0o755, 0));
        assert!(entries(&a).unwrap().is_empty());
    }

    #[test]
    fn no_panic_on_garbage() {
        // Every single-byte corruption of a small archive parses or errors.
        let mut a = header("d/", "", b'5', 0o755, 0);
        a.extend(file("d/f", b"hello"));
        let a = end(a);
        for i in 0..a.len() {
            for v in [0u8, b'0', b'7', b' ', 0x80, 0xff] {
                let mut b = a.clone();
                b[i] = v;
                let _ = entries(&b);
                let _ = entries(&b[..i]);
            }
        }
    }
}
