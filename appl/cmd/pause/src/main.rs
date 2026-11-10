//! pause: never return. Used to keep a shell from reading more input.

use std::io::Read;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.len() > 1 {
        return Err(Error::from("usage: pause"));
    }
    // A read on a pipe nobody writes blocks in the kernel, where a kill
    // can still reach it.
    let [r, _w] = purgatorio::pipe()?;
    let mut r = std::fs::File::from(r);
    let mut b = [0u8; 1];
    loop {
        // A kill ends the read; leave quietly.
        if let Err(e) = r.read(&mut b) {
            purgatorio::exits(&Error::from(e).0);
        }
    }
}
