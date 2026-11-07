//! env: print the environment group's variables as `name=value`, sorted by
//! name.

use std::io::Write;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    let mut out = String::new();
    for (k, v) in purgatorio::env::list()? {
        out.push_str(&format!("{k}={v}\n"));
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
