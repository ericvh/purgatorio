//! unmount [source] target: undo a bind. With `source`, only that member is
//! removed from the union at `target`; otherwise everything bound there is.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (source, target) = match &args[1..] {
        [target] => (None, target),
        [source, target] => (Some(source.as_str()), target),
        _ => return Err(Error::from("usage: unmount [source] target")),
    };
    purgatorio::unmount(source, target).map_err(|e| Error(format!("{target}: {e}")))
}
