//! cmp a b: report the first byte where two files differ.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [a, b] = args
        .get(1..3)
        .ok_or_else(|| Error::from("usage: cmp a b"))?
    else {
        return Err(Error::from("usage: cmp a b"));
    };
    let a = std::fs::read(a).map_err(|_| Error::last())?;
    let b = std::fs::read(b).map_err(|_| Error::last())?;
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return Err(Error(format!("cmp: files differ: byte {}", i + 1)));
        }
    }
    if a.len() != b.len() {
        return Err(Error(format!("cmp: EOF at byte {}", n + 1)));
    }
    Ok(())
}
