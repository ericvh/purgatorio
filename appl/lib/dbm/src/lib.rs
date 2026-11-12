//! Hashed databases of keys and values, laid out as dbm keeps them:
//! `name.dir` is a bit map saying which blocks have been split, and
//! `name.pag` holds 512-byte blocks of pairs. A key's hash picks its
//! block; a block that fills is split in two by the next bit of the
//! hash. The hash is this library's own (FNV-1a), so databases made by
//! Inferno's Dbm module cannot be read here, nor these there.
//!
//! A block is a count `n` (u16), `n` offsets (u16) and the items, which
//! are packed from the end of the block down: item `i` runs from offset
//! `i` to offset `i - 1` (the block's end for the first). Items go in
//! pairs, key then value. Integers are little-endian. A key and its
//! value must fit in one block.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

pub const PBLKSIZ: usize = 512;
const DBLKSIZ: usize = 4096;

pub const ETOOBIG: &str = "key and value too big for a block";

fn hash(key: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in key {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Where the blocks and the bit map are kept.
pub trait Store {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String>;
    fn size(&mut self) -> Result<u64, String>;
}

impl Store for File {
    /// Past the end of the file reads as zeros.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        self.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
        let mut got = 0;
        while got < buf.len() {
            match self.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        self.seek(SeekFrom::Start(off))
            .and_then(|_| self.write_all(buf))
            .map_err(|e| e.to_string())
    }

    fn size(&mut self) -> Result<u64, String> {
        self.seek(SeekFrom::End(0)).map_err(|e| e.to_string())
    }
}

impl Store for Vec<u8> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), String> {
        buf.fill(0);
        let off = off as usize;
        if off < self.len() {
            let n = buf.len().min(self.len() - off);
            buf[..n].copy_from_slice(&self[off..off + n]);
        }
        Ok(())
    }

    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<(), String> {
        let off = off as usize;
        if self.len() < off + buf.len() {
            self.resize(off + buf.len(), 0);
        }
        self[off..off + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn size(&mut self) -> Result<u64, String> {
        Ok(Vec::len(self) as u64)
    }
}

/// A block's items.
fn items(b: &[u8; PBLKSIZ]) -> Vec<&[u8]> {
    let n = usize::from(u16::from_le_bytes([b[0], b[1]]));
    let mut out = Vec::new();
    let mut end = PBLKSIZ;
    for i in 0..n {
        let at = 2 + 2 * i;
        if at + 2 > PBLKSIZ {
            break;
        }
        let start = usize::from(u16::from_le_bytes([b[at], b[at + 1]]));
        if start > end || start < 2 + 2 * n {
            break;
        }
        out.push(&b[start..end]);
        end = start;
    }
    // A damaged block keeps its whole pairs only.
    out.truncate(out.len() & !1);
    out
}

/// A block holding `items`, or `None` when they do not fit.
fn pack(items: &[&[u8]]) -> Option<[u8; PBLKSIZ]> {
    let n = items.len();
    let need = 2 + 2 * n + items.iter().map(|i| i.len()).sum::<usize>();
    if need > PBLKSIZ {
        return None;
    }
    let mut b = [0u8; PBLKSIZ];
    b[..2].copy_from_slice(&(n as u16).to_le_bytes());
    let mut end = PBLKSIZ;
    for (i, it) in items.iter().enumerate() {
        let start = end - it.len();
        b[start..end].copy_from_slice(it);
        b[2 + 2 * i..4 + 2 * i].copy_from_slice(&(start as u16).to_le_bytes());
        end = start;
    }
    Some(b)
}

/// A key and its value.
pub type Pair = (Vec<u8>, Vec<u8>);

pub struct Db<S: Store> {
    dir: S,
    pag: S,
    rdonly: bool,
}

/// What `store` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    Done,
    /// The key was there and `replace` was not given.
    Exists,
}

impl Db<File> {
    /// Opens the database `name` (`name.dir` and `name.pag`), for
    /// writing too unless `rdonly`.
    pub fn open(name: &str, rdonly: bool) -> std::io::Result<Db<File>> {
        let open = |ext: &str| {
            OpenOptions::new()
                .read(true)
                .write(!rdonly)
                .open(format!("{name}.{ext}"))
        };
        Ok(Db {
            dir: open("dir")?,
            pag: open("pag")?,
            rdonly,
        })
    }

    /// Makes the database `name` empty, creating its files.
    pub fn create(name: &str) -> std::io::Result<Db<File>> {
        for ext in ["dir", "pag"] {
            File::create(format!("{name}.{ext}"))?;
        }
        Db::open(name, false)
    }
}

impl<S: Store> Db<S> {
    pub fn new(dir: S, pag: S) -> Db<S> {
        Db {
            dir,
            pag,
            rdonly: false,
        }
    }

    fn bit(&mut self, n: u64) -> Result<bool, String> {
        let mut b = [0u8];
        self.dir.read_at(n / 8, &mut b)?;
        Ok(b[0] & (1 << (n % 8)) != 0)
    }

    fn set_bit(&mut self, n: u64) -> Result<(), String> {
        if n / 8 >= DBLKSIZ as u64 * 1024 {
            return Err(String::from(ETOOBIG));
        }
        let mut b = [0u8];
        self.dir.read_at(n / 8, &mut b)?;
        b[0] |= 1 << (n % 8);
        self.dir.write_at(n / 8, &b)
    }

    /// The block for `h`, and the mask of hash bits that chose it.
    fn locate(&mut self, h: u32) -> Result<(u64, u64), String> {
        let mut mask: u64 = 0;
        loop {
            let blk = u64::from(h) & mask;
            if mask >= 0xffff_ffff || !self.bit(blk + mask)? {
                return Ok((blk, mask));
            }
            mask = mask << 1 | 1;
        }
    }

    fn block(&mut self, n: u64) -> Result<[u8; PBLKSIZ], String> {
        let mut b = [0u8; PBLKSIZ];
        self.pag.read_at(n * PBLKSIZ as u64, &mut b)?;
        Ok(b)
    }

    fn put_block(&mut self, n: u64, b: &[u8; PBLKSIZ]) -> Result<(), String> {
        self.pag.write_at(n * PBLKSIZ as u64, b)
    }

    pub fn fetch(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let (blk, _) = self.locate(hash(key))?;
        let b = self.block(blk)?;
        Ok(items(&b)
            .chunks(2)
            .find(|p| p[0] == key)
            .map(|p| p[1].to_vec()))
    }

    /// Stores `value` under `key`; an existing key is replaced only with
    /// `replace`.
    pub fn store(&mut self, key: &[u8], value: &[u8], replace: bool) -> Result<Stored, String> {
        if self.rdonly {
            return Err(String::from("database is read-only"));
        }
        if 2 + 4 + key.len() + value.len() > PBLKSIZ {
            return Err(String::from(ETOOBIG));
        }
        let h = hash(key);
        loop {
            let (blk, mask) = self.locate(h)?;
            let b = self.block(blk)?;
            let mut all: Vec<&[u8]> = items(&b);
            if let Some(i) = all.chunks(2).position(|p| p[0] == key) {
                if !replace {
                    return Ok(Stored::Exists);
                }
                all.drain(2 * i..2 * i + 2);
            }
            all.push(key);
            all.push(value);
            if let Some(nb) = pack(&all) {
                self.put_block(blk, &nb)?;
                return Ok(Stored::Done);
            }
            // Split the block by the next bit of the hash.
            if mask >= 0x7fff_ffff {
                return Err(String::from(ETOOBIG));
            }
            let old: Vec<&[u8]> = items(&b);
            let bit = mask + 1;
            let (mut stay, mut go): (Vec<&[u8]>, Vec<&[u8]>) = (Vec::new(), Vec::new());
            for p in old.chunks(2) {
                if u64::from(hash(p[0])) & bit != 0 {
                    go.extend_from_slice(p);
                } else {
                    stay.extend_from_slice(p);
                }
            }
            let (s, g) = (pack(&stay), pack(&go));
            let (Some(s), Some(g)) = (s, g) else {
                return Err(String::from(ETOOBIG));
            };
            self.put_block(blk, &s)?;
            self.put_block(blk + bit, &g)?;
            self.set_bit(blk + mask)?;
        }
    }

    /// Deletes `key`; false when it was not there.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        if self.rdonly {
            return Err(String::from("database is read-only"));
        }
        let (blk, _) = self.locate(hash(key))?;
        let b = self.block(blk)?;
        let mut all = items(&b);
        let Some(i) = all.chunks(2).position(|p| p[0] == key) else {
            return Ok(false);
        };
        all.drain(2 * i..2 * i + 2);
        let nb = pack(&all).expect("fewer items fit");
        self.put_block(blk, &nb)?;
        Ok(true)
    }

    /// Every key and value, block by block.
    pub fn pairs(&mut self) -> Result<Vec<Pair>, String> {
        let blocks = self.pag.size()?.div_ceil(PBLKSIZ as u64);
        let mut out = Vec::new();
        for n in 0..blocks {
            let b = self.block(n)?;
            for p in items(&b).chunks(2) {
                out.push((p[0].to_vec(), p[1].to_vec()));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many() {
        let mut db = Db::new(Vec::new(), Vec::new());
        for i in 0..2000u32 {
            let k = format!("key{i}");
            let v = format!("value number {i}");
            assert_eq!(
                db.store(k.as_bytes(), v.as_bytes(), false).unwrap(),
                Stored::Done
            );
        }
        assert_eq!(db.store(b"key7", b"x", false).unwrap(), Stored::Exists);
        assert_eq!(db.fetch(b"key7").unwrap().unwrap(), b"value number 7");
        db.store(b"key7", b"seven", true).unwrap();
        assert_eq!(db.fetch(b"key7").unwrap().unwrap(), b"seven");
        for i in (0..2000u32).step_by(3) {
            assert!(db.delete(format!("key{i}").as_bytes()).unwrap());
        }
        assert!(!db.delete(b"key0").unwrap());
        assert!(db.fetch(b"key3").unwrap().is_none());
        assert_eq!(db.fetch(b"key1999").unwrap().unwrap(), b"value number 1999");
        let all = db.pairs().unwrap();
        assert_eq!(all.len(), 2000 - 667);
        assert!(db.store(b"k", &[0; 600], false).is_err());
        // Keys and values may hold any bytes.
        db.store(b"\0\n\t", b"\xff\0", false).unwrap();
        assert_eq!(db.fetch(b"\0\n\t").unwrap().unwrap(), b"\xff\0");
    }

    #[test]
    fn damage() {
        let mut b = [0u8; PBLKSIZ];
        b[0] = 200;
        assert!(items(&b).is_empty());
        let mut db = Db::new(vec![0xff; 16], vec![0x7f; 4096]);
        let _ = db.fetch(b"x");
        let _ = db.pairs();
        let _ = db.store(b"x", b"y", false);
    }
}
