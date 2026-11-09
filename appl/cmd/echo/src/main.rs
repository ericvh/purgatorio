//! echo [-n] [arg ...]: write the arguments, separated by spaces and followed
//! by a newline unless -n is given, in a single write.

use std::io::Write;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut words = &args[1.min(args.len())..];
    let newline = words.first().map(String::as_str) != Some("-n");
    if !newline {
        words = &words[1..];
    }
    let mut out = words.join(" ");
    if newline {
        out.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
