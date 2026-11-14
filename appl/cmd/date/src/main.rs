//! date [-nu] [seconds]: print the date, or the given time, in local time
//! (daytime(2): `$timezone`, else `/locale/timezone`); `-u` prints it in
//! GMT, and `-n` prints seconds since the epoch instead.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error, daytime};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "date [-nu] [seconds]");
    let (mut numeric, mut utc) = (false, false);
    while let Some(c) = arg.opt() {
        match c {
            'n' => numeric = true,
            'u' => utc = true,
            _ => return Err(arg.usage()),
        }
    }
    let t = match arg.argv().first() {
        Some(s) => s.parse().map_err(|_| Error(format!("bad time {s:?}")))?,
        None => daytime::now(),
    };
    if numeric {
        println!("{t}");
    } else if utc {
        println!("{}", daytime::text(&daytime::gmt(t)));
    } else {
        println!("{}", daytime::text(&daytime::local(t)));
    }
    Ok(())
}
