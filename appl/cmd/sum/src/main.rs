//! sum [file ...]: print a 32-bit CRC, the length in bytes and the name of
//! each file (directories are skipped), or of standard input. The CRC
//! covers the data and then four bytes encoding its length, so that empty
//! files do not sum to zero.

use std::io::{self, Read};

use purgatorio::{Context, DMDIR, Error};

struct Crc([u32; 256]);

impl Crc {
    fn new() -> Crc {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xedb8_8320
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        Crc(t)
    }

    fn update(&self, mut crc: u32, data: &[u8]) -> u32 {
        for &b in data {
            crc = self.0[((crc as u8) ^ b) as usize] ^ (crc >> 8);
        }
        crc
    }
}

fn sum(crc: &Crc, r: &mut dyn Read) -> io::Result<(u32, u64)> {
    let mut c = 0;
    let mut n = 0u64;
    let mut buf = vec![0u8; 8192];
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        c = crc.update(c, &buf[..k]);
        n += k as u64;
    }
    let l = n as u32;
    let tail = [
        ((l >> 24) as u8) ^ 0xcc,
        ((l >> 16) as u8) ^ 0x55,
        ((l >> 8) as u8) ^ 0xcc,
        (l as u8) ^ 0x55,
    ];
    Ok((crc.update(c, &tail), n))
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let crc = Crc::new();
    if args.len() < 2 {
        let (c, n) = sum(&crc, &mut io::stdin().lock())?;
        println!("{c:08x} {n:6}");
        return Ok(());
    }
    let mut failed = false;
    for f in &args[1..] {
        match purgatorio::stat(f) {
            Ok(d) if d.mode & DMDIR != 0 => continue,
            Ok(_) => {}
            Err(e) => {
                eprintln!("sum: cannot get status of {f}: {e}");
                failed = true;
                continue;
            }
        }
        let mut file = match std::fs::File::open(f) {
            Ok(file) => file,
            Err(_) => {
                eprintln!("sum: cannot open {f}: {}", purgatorio::errstr());
                failed = true;
                continue;
            }
        };
        match sum(&crc, &mut file) {
            Ok((c, n)) => println!("{c:08x} {n:6} {f}"),
            Err(e) => {
                eprintln!("sum: error reading {f}: {}", Error::from(e));
                failed = true;
            }
        }
    }
    if failed {
        purgatorio::exits("fail:error");
    }
    Ok(())
}
