//! styxmon [-d] cmd [arg ...]: run `cmd` with sh, a 9P server speaking on
//! its standard input and output, and pass the 9P conversation on
//! standard input to it, printing every message on standard error in the
//! text form of Inferno's Styx module, in the order they pass. `-d` also
//! prints the data of reads and writes. For example
//! `mount {styxmon export /} /n/remote`.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::abi::pctl;
use purgatorio::arg::Arg;
use purgatorio::{Context, Error};
use styxserv::text;

/// Copies whole messages from `from` to `to`, printing each; returns at
/// end of file or error.
fn relay(from: &mut File, to: &mut File, data: bool, who: &str) -> Result<(), String> {
    loop {
        let m = match styxserv::read_msg(from) {
            Ok(Some(m)) => m,
            Ok(None) => return Err(format!("eof from {who}")),
            Err(e) => return Err(format!("error from {who}: {}", styxserv::ioerr(e))),
        };
        {
            let mut err = io::stderr().lock();
            match styxserv::decode(&m) {
                Ok(msg) => {
                    let _ = writeln!(err, "{}", text::msg(&msg));
                    if data && let Some(d) = text::payload(&msg) {
                        let _ = writeln!(err, "\t{}", text::data(d));
                    }
                }
                Err(e) => {
                    let _ = writeln!(err, "styxmon: message error ({e}, {} bytes)", m.len());
                }
            }
        }
        to.write_all(&m)
            .map_err(|e| format!("error to {who}: {}", styxserv::ioerr(e)))?;
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--relay") {
        // The server-to-client half: fd 0 is the server, fd 1 the client.
        let data = args.get(2).map(String::as_str) == Some("-d");
        // SAFETY: the parent gave this prog exactly these fds.
        let (mut from, mut to) = unsafe { (File::from_raw_fd(0), File::from_raw_fd(1)) };
        let _ = relay(&mut from, &mut to, data, "server");
        return Ok(());
    }
    let mut arg = Arg::new(args, "styxmon [-d] cmd [arg...]");
    let mut data = false;
    while let Some(c) = arg.opt() {
        match c {
            'd' => data = true,
            _ => return Err(arg.usage()),
        }
    }
    let cmd = arg.argv();
    if cmd.is_empty() {
        return Err(Error::from("usage: styxmon [-d] cmd [arg...]"));
    }
    let [srv, ours] = purgatorio::pipe()?;
    let (s, o) = (srv.as_raw_fd(), ours.as_raw_fd());
    purgatorio::spawn(
        "/bin/sh.wasm",
        &[String::from("sh"), String::from("-c"), cmd.join(" ")],
        pctl::NEWFD,
        &[(s, 0), (s, 1), (2, 2)],
    )
    .map_err(|e| Error(format!("cannot load {}: {e}", cmd[0])))?;
    drop(srv);
    let mut argv = vec![String::from("styxmon"), String::from("--relay")];
    if data {
        argv.push(String::from("-d"));
    }
    let child = purgatorio::spawn(
        "/bin/styxmon.wasm",
        &argv,
        pctl::NEWFD,
        &[(o, 0), (0, 1), (2, 2)],
    )?;
    // SAFETY: fd 0 is the client's conversation; this prog owns it now.
    let mut client = unsafe { File::from_raw_fd(0) };
    let mut server = File::from(ours);
    let r = relay(&mut client, &mut server, data, "client");
    // The relay child holds the server's pipe open; stop it so the
    // server sees the end of the conversation.
    if let Ok(mut ctl) = std::fs::OpenOptions::new()
        .write(true)
        .open(format!("/prog/{child}/ctl"))
    {
        let _ = ctl.write_all(b"kill");
    }
    match r {
        Err(e) if e.starts_with("eof") => Ok(()),
        Err(e) => Err(Error(e)),
        Ok(()) => Ok(()),
    }
}
