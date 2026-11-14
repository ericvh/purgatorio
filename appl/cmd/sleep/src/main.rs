//! sleep seconds: wait. Fractions such as `0.25` are allowed. A time
//! that is not a number is a usage error, as in Inferno's sleep.

use std::time::Duration;

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let usage = || Error::from("usage: sleep seconds");
    let [_, secs] = args.as_slice() else {
        return Err(usage());
    };
    let secs: f64 = secs
        .parse()
        .ok()
        .filter(|s: &f64| s.is_finite() && *s >= 0.0)
        .ok_or_else(usage)?;
    std::thread::sleep(Duration::from_secs_f64(secs));
    Ok(())
}
