//! basename [-d] string [suffix]: print `string` without any prefix ending
//! in `/` and without `suffix`. `-d` prints the directory part instead:
//! everything before the last `/`, or `.` when there is none.

use purgatorio::arg::Arg;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "basename [-d] string [suffix]");
    let mut dir = false;
    while let Some(o) = arg.opt() {
        match o {
            'd' => dir = true,
            _ => return Err(arg.usage()),
        }
    }
    let usage = arg.usage();
    let words = arg.argv();
    let (s, suffix) = match words.as_slice() {
        [s] => (s.as_str(), None),
        [s, x] if !dir => (s.as_str(), Some(x.as_str())),
        _ => return Err(usage),
    };
    if dir {
        match s.rfind('/') {
            Some(i) => println!("{}", &s[..i]),
            None => println!("."),
        }
        return Ok(());
    }
    let mut base = s.rsplit('/').next().unwrap_or(s);
    if let Some(x) = suffix
        && base.len() > x.len()
        && let Some(b) = base.strip_suffix(x)
    {
        base = b;
    }
    println!("{base}");
    Ok(())
}
