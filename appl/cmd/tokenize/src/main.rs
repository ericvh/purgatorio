//! tokenize string delimiters: print the words of `string`, split at any
//! of the characters in `delimiters`, one per line. Empty words are
//! dropped.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [_, s, delims] = args.as_slice() else {
        // As Inferno's tokenize: `Usage:`, and `fail: usage` with a space.
        eprintln!("Usage: tokenize string delimiters");
        purgatorio::exits("fail: usage");
    };
    for w in s.split(|c| delims.contains(c)).filter(|w| !w.is_empty()) {
        println!("{w}");
    }
    Ok(())
}
