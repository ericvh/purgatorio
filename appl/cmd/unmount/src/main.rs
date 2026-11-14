//! unmount [source] target: undo a bind. With `source`, only that member is
//! removed from the union at `target`; otherwise everything bound there is.
//! As in Inferno, neither a bad command line nor a failure sets a status.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let (source, target) = match &args[1..] {
        [target] => (None, target),
        [source, target] => (Some(source.as_str()), target),
        _ => {
            eprintln!("Usage: unmount [source] target");
            return Ok(());
        }
    };
    // As Inferno's unmount: the error is printed and the status is empty.
    if let Err(e) = purgatorio::unmount(source, target) {
        eprintln!("unmount: {target}: {e}");
    }
    Ok(())
}
