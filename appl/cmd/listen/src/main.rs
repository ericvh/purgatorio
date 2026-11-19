//! listen [-A] addr cmd [args...]: like styxlisten, and print each remote.
//!
//! As in Inferno's listen(1), `cmd` may be a `{command}` block, which sh
//! runs with the connection as its standard input and output.

use purgatorio::eprintln;
use purgatorio::fd::AsRawFd;

use purgatorio::abi::pctl::NEWFD;
use purgatorio::arg::Arg;
use purgatorio::dial;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(args, "listen [-A] addr cmd [args...]");
    while let Some(c) = arg.opt() {
        if c != 'A' {
            return Err(arg.usage());
        }
    }
    let rest = arg.argv();
    if rest.len() < 2 {
        return Err(Error::from("usage: listen [-A] addr cmd [args...]"));
    }
    let ann = dial::announce(&rest[0])?;
    let cmd = rest[1].clone();
    let argv = rest[1..].to_vec();
    loop {
        let conn = dial::listen(&ann.dir)?;
        let remote =
            purgatorio::fs::read_to_string(format!("{}/remote", conn.dir)).unwrap_or_default();
        eprintln!("{}", remote.trim());
        let Some(data) = conn.data.as_ref() else {
            continue;
        };
        match cmd.strip_prefix('{').and_then(|c| c.strip_suffix('}')) {
            Some(block) => purgatorio::spawn(
                "/bin/sh.wasm",
                &[String::from("sh"), String::from("-c"), String::from(block)],
                NEWFD,
                &[(data.as_raw_fd(), 0), (data.as_raw_fd(), 1), (2, 2)],
            )?,
            None => purgatorio::spawn(&cmd, &argv, NEWFD, &[(data.as_raw_fd(), 0)])?,
        };
    }
}
