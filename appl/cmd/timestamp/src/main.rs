//! timestamp [tag]: copy standard input to standard output a line at a
//! time, each line prefixed by the milliseconds since `timestamp` started
//! and by `tag` if given. The first line gives the start time in
//! milliseconds since the epoch.

use std::io::{BufRead, Write};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let tag = match args.as_slice() {
        [_] => String::new(),
        [_, t] => format!("{t} "),
        _ => return Err(Error::from("usage: timestamp [tag]")),
    };
    let start = Instant::now();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let mut out = std::io::stdout().lock();
    writeln!(out, "{:010} {tag}start {epoch}", 0)?;
    out.flush()?;
    let mut input = std::io::stdin().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') {
            line.push(b'\n');
        }
        let ms = start.elapsed().as_millis();
        write!(out, "{ms:010} {tag}")?;
        out.write_all(&line)?;
        out.flush()?;
    }
}
