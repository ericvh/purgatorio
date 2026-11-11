//! ns [-r] [pid]: print a prog's namespace as the bind and mount commands
//! that would build it, then `cd`, as Inferno's ns does. The prog is this
//! one unless `pid` is given. A mount on a network connection shows the
//! connection's address, unless `-r` is given.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, pid};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "ns [-r] [pid]");
    let mut raw = false;
    while let Some(c) = arg.opt() {
        match c {
            'r' => raw = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let id = match arg.argv().as_slice() {
        [] => pid(),
        [p] => p.parse().unwrap_or(0),
        _ => return Err(usage),
    };
    let path = format!("/prog/{id}/ns");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| Error(format!("can't open {path}: {}", Error::last())))?;
    print!("{}", purgatorio::ns::tidy(&text, !raw));
    Ok(())
}
