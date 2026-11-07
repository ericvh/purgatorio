//! date [-nu] [seconds]: print the date, or the given time, in GMT (so
//! `-u` changes nothing yet). `-n` prints seconds since the epoch instead.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, daytime};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "date [-nu] [seconds]");
    let mut numeric = false;
    while let Some(c) = arg.opt() {
        match c {
            'n' => numeric = true,
            'u' => {}
            _ => return Err(arg.usage()),
        }
    }
    let t = match arg.argv().first() {
        Some(s) => s.parse().map_err(|_| Error(format!("bad time {s:?}")))?,
        None => daytime::now(),
    };
    if numeric {
        println!("{t}");
    } else {
        println!("{}", daytime::text(&daytime::gmt(t)));
    }
    Ok(())
}
