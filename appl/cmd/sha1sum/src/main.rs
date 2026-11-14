//! sha1sum [file ...]: print the SHA1 hash of each file and its name,
//! separated by a tab, or just the hash of standard input.

use std::io::{self, Read};

use purgatorio::{Context, Error};

/// An incremental SHA1 (FIPS 180-4).
struct Sha1 {
    h: [u32; 5],
    block: Vec<u8>,
    len: u64,
}

impl Sha1 {
    fn new() -> Sha1 {
        Sha1 {
            h: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            block: Vec::with_capacity(64),
            len: 0,
        }
    }

    fn compress(&mut self, chunk: &[u8]) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (h, v) in self.h.iter_mut().zip([a, b, c, d, e]) {
            *h = h.wrapping_add(v);
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.block.len()).min(data.len());
            self.block.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.block.len() == 64 {
                let b = std::mem::take(&mut self.block);
                self.compress(&b);
                self.block = b;
                self.block.clear();
            }
        }
    }

    fn finish(mut self) -> [u8; 20] {
        let bits = self.len.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        while (self.block.len() + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&bits.to_be_bytes());
        let len = self.len;
        self.update(&pad);
        self.len = len;
        let mut out = [0u8; 20];
        for (o, h) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            o.copy_from_slice(&h.to_be_bytes());
        }
        out
    }
}

fn hash(r: &mut dyn Read) -> io::Result<String> {
    let mut s = Sha1::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        s.update(&buf[..n]);
    }
    Ok(s.finish().iter().map(|b| format!("{b:02x}")).collect())
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() < 2 {
        println!("{}", hash(&mut io::stdin().lock())?);
        return Ok(());
    }
    let mut failed = false;
    for f in &args[1..] {
        let mut file = match std::fs::File::open(f) {
            Ok(file) => file,
            Err(_) => {
                eprintln!("sha1sum: cannot open {f}: {}", purgatorio::errstr());
                failed = true;
                continue;
            }
        };
        match hash(&mut file) {
            Ok(h) => println!("{h}\t{f}"),
            Err(e) => {
                eprintln!("sha1sum: error reading {f}: {}", Error::from(e));
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:error");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn vectors() {
        let h = |s: &[u8]| hash(&mut &s[..]).unwrap();
        assert_eq!(h(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(h(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let long = vec![b'a'; 1_000_000];
        assert_eq!(h(&long), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    }
}
