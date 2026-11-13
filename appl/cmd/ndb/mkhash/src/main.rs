//! ndb/mkhash file attr: write `file.attr`, a hash table of the values of
//! `attr` in the attrdb(6) file `file`, in the format of Plan 9's and
//! Inferno's ndb/mkhash.
//!
//! The table starts with the file's modification time and the table's
//! length `hlen` (twice the number of values, plus one), four bytes each,
//! least significant first. Then come `hlen` three-byte slots and the
//! collision chains. A value hashes to the slot `h mod hlen`, with `h`
//! the 32-bit sum `h*13 + byte - 'a'` over its bytes. A slot holds the
//! offset of the entry with the value, or `0x800001` when empty; a slot
//! with bit 23 set points at a chain pair (an entry's offset, then the
//! next slot). The attrdb readers here search the text and do not use
//! the table: it is for other systems that share the database.

use std::io::Write;

use purgatorio::{Context, Error};

const NDBPLEN: usize = 3;
const NDBHLEN: usize = 8;
const NDBCHAIN: u32 = 1 << 23;
const NDBNAP: u32 = NDBCHAIN | 1;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, dbname, attr] = args.as_slice() else {
        fail("usage: mkhash file attr");
    };
    let text = match std::fs::read(dbname) {
        Ok(t) => t,
        Err(e) => fail(&format!("can't open {dbname}: {}", Error::from(e))),
    };
    let mtime = purgatorio::stat(dbname).map_or(0, |d| d.mtime);
    let file = build(&text, attr, mtime);
    let hashfile = format!("{dbname}.{attr}");
    let mut f = match purgatorio::create(&hashfile, purgatorio::abi::mode::OWRITE, 0o666) {
        Ok(fd) => std::fs::File::from(fd),
        Err(e) => fail(&format!("can't create {hashfile}: {e}")),
    };
    if let Err(e) = f.write_all(&file) {
        fail(&format!("error writing {hashfile}: {}", Error::from(e)));
    }
    Ok(())
}

fn fail(msg: &str) -> ! {
    eprintln!("mkhash: {msg}");
    purgatorio::exits("fail:error");
}

/// The entries of attrdb text: each one's offset and the values of
/// `attr` in it.
fn entries(text: &[u8], attr: &str) -> Vec<(u32, Vec<String>)> {
    let mut out: Vec<(u32, Vec<String>)> = Vec::new();
    let mut off = 0usize;
    for raw in text.split_inclusive(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim_end_matches('\n');
        let pairs = ndb::parse_line(line).unwrap_or_default();
        let cont = line.starts_with([' ', '\t']);
        if !pairs.is_empty() {
            if !cont || out.is_empty() {
                out.push((off as u32, Vec::new()));
            }
            if let Some(e) = out.last_mut() {
                e.1.extend(pairs.into_iter().filter(|p| p.attr == attr).map(|p| p.val));
            }
        }
        off += raw.len();
    }
    out
}

fn hash(s: &str, hlen: u32) -> u32 {
    let mut h: u32 = 0;
    for b in s.bytes() {
        h = h
            .wrapping_mul(13)
            .wrapping_add(u32::from(b))
            .wrapping_sub(u32::from(b'a'));
    }
    h % hlen
}

fn get3(a: &[u8]) -> u32 {
    u32::from(a[0]) | u32::from(a[1]) << 8 | u32::from(a[2]) << 16
}

fn put3(a: &mut [u8], v: u32) {
    a[..3].copy_from_slice(&v.to_le_bytes()[..3]);
}

/// The hash file for the values of `attr` in `text`.
fn build(text: &[u8], attr: &str, mtime: u32) -> Vec<u8> {
    let ents = entries(text, attr);
    let n: usize = ents.iter().map(|(_, v)| v.len()).sum();
    let hlen = 2 * n + 1;
    let mut tab = vec![0u8; hlen * NDBPLEN + n * 2 * NDBPLEN];
    for i in (0..hlen * NDBPLEN).step_by(NDBPLEN) {
        put3(&mut tab[i..], NDBNAP);
    }
    let mut chain = hlen * NDBPLEN;
    for (offset, vals) in &ents {
        for v in vals {
            let mut o = hash(v, hlen as u32) as usize * NDBPLEN;
            loop {
                let p = get3(&tab[o..]);
                if p == NDBNAP {
                    put3(&mut tab[o..], *offset);
                    break;
                }
                if p & NDBCHAIN == 0 {
                    put3(&mut tab[o..], chain as u32 | NDBCHAIN);
                    put3(&mut tab[chain..], p);
                    put3(&mut tab[chain + NDBPLEN..], *offset);
                    chain += 2 * NDBPLEN;
                    break;
                }
                o = (p & !NDBCHAIN) as usize;
            }
        }
    }
    let mut file = Vec::with_capacity(NDBHLEN + chain);
    file.extend_from_slice(&mtime.to_le_bytes());
    file.extend_from_slice(&(hlen as u32).to_le_bytes());
    file.extend_from_slice(&tab[..chain]);
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table() {
        let db = b"# c\nsys=a ip=1\n\tip=2\nsys=b ip=1\n";
        let e = entries(db, "ip");
        assert_eq!(
            e,
            [
                (4, vec![String::from("1"), String::from("2")]),
                (21, vec![String::from("1")])
            ]
        );
        let f = build(db, "ip", 7);
        assert_eq!(&f[..8], &[7, 0, 0, 0, 7, 0, 0, 0]);
        // "1" and "1" collide: a chain holding the second entry first.
        let slot = hash("1", 7) as usize * 3 + 8;
        let p = get3(&f[slot..]);
        assert_ne!(p & NDBCHAIN, 0);
        let c = (p & !NDBCHAIN) as usize + 8;
        assert_eq!(get3(&f[c..]), 4);
        assert_eq!(get3(&f[c + 3..]), 21);
        assert_eq!(f.len(), 8 + 7 * 3 + 2 * 3);
    }
}
