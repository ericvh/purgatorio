//! styxlisten [-A] addr cmd [args...]: announce `addr` and, for each
//! connection, run `cmd` with the connection as its standard input.

use std::os::fd::AsRawFd;

use purgatorio::abi::pctl::NEWFD;
use purgatorio::arg::Arg;
use purgatorio::dial::{self, local};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "styxlisten [-A] addr cmd [args...]");
    let mut noauth = false;
    while let Some(c) = arg.opt() {
        if c != 'A' {
            return Err(arg.usage());
        }
        noauth = true;
    }
    let rest = arg.argv();
    if rest.len() < 2 {
        return Err(Error::from("usage: styxlisten [-A] addr cmd [args...]"));
    }
    let ann = dial::announce(&rest[0])?;
    let here = local(&ann.dir).unwrap_or_default();
    eprintln!("styxlisten: {here}");
    let cmd = rest[1].clone();
    let mut argv = rest[1..].to_vec();
    if noauth {
        argv.insert(1, String::from("-A"));
    }
    loop {
        let conn = dial::listen(&ann.dir)?;
        let Some(data) = conn.data.as_ref() else {
            continue;
        };
        purgatorio::spawn(&cmd, &argv, NEWFD, &[(data.as_raw_fd(), 0)])?;
    }
}
