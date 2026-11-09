//! export [dir]: serve `dir` (default `/`) as 9P on file descriptor 0.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let dir = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("/");
    purgatorio::export(0, dir, 0).map_err(|e| Error(format!("export: {e}")))
}
