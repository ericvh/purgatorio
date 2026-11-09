//! ns [pid]: print the namespace as bind and mount commands.

use purgatorio::{Context, Error, pid};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let id = match args.get(1) {
        Some(s) => s.parse().map_err(|_| Error::from("bad pid"))?,
        None => pid(),
    };
    let path = format!("/prog/{id}/ns");
    print!(
        "{}",
        std::fs::read_to_string(&path).map_err(|_| Error::last())?
    );
    Ok(())
}
