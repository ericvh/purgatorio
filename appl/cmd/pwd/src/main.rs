//! pwd: print the current directory.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    println!("{}", purgatorio::dot()?);
    Ok(())
}
