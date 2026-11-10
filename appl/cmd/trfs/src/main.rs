//! trfs olddir mountdir: mount `olddir` on `mountdir` with every space in
//! a file name shown as a no-break space (U+00A0), so names with spaces
//! are one word to the shell. Names given to the mounted tree are
//! translated back. A name that already holds a no-break space cannot be
//! told apart from one with a space.

use purgatorio::abi::mount::{MCREATE, MREPL};
use purgatorio::{Context, Error};
use styxserv::pass::{Pass, Policy};

const NBSP: char = '\u{a0}';

struct Spaces;

impl Policy for Spaces {
    fn to_inner(&self, name: &str) -> String {
        name.replace(NBSP, " ")
    }
    fn to_outer(&self, name: &str) -> String {
        name.replace(' ', "\u{a0}")
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--serve") {
        let dir = args
            .get(2)
            .ok_or_else(|| Error::from("usage: trfs olddir mountdir"))?;
        return styxserv::bg::serve(&mut Pass::new(dir, Spaces));
    }
    let [_, old, new] = args.as_slice() else {
        return Err(Error::from("usage: trfs olddir mountdir"));
    };
    let old = styxserv::abs(old);
    purgatorio::stat(&old).map_err(|e| Error(format!("{old}: {e}")))?;
    styxserv::bg::mount(
        "/bin/trfs.wasm",
        &[String::from("trfs"), String::from("--serve"), old],
        new,
        MREPL | MCREATE,
    )
    .map_err(|e| Error(format!("cannot mount on {new}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(Spaces.to_outer("a b c"), "a\u{a0}b\u{a0}c");
        assert_eq!(Spaces.to_inner("a\u{a0}b"), "a b");
    }
}
