//! cmp a b: report the first byte where two files differ. The status is
//! `fail:differ`, `fail:EOF` when one file is a prefix of the other, or
//! `fail:open`, as in cmp(1).

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [a, b] = args
        .get(1..3)
        .ok_or_else(|| Error::from("usage: cmp a b"))?
    else {
        return Err(Error::from("usage: cmp a b"));
    };
    let (a, b) = (read(a), read(b));
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            eprintln!("cmp: files differ: byte {}", i + 1);
            purgatorio::exits("fail:differ");
        }
    }
    if a.len() != b.len() {
        eprintln!("cmp: EOF at byte {}", n + 1);
        purgatorio::exits("fail:EOF");
    }
    Ok(())
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|_| {
        eprintln!("cmp: can't open {path}: {}", Error::last());
        purgatorio::exits("fail:open");
    })
}
