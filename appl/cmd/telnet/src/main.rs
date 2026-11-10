//! telnet machine: connect to `machine` (any dial address; the network
//! defaults to `tcp` and the port to 23) and speak the Telnet protocol
//! (RFC 854): standard input goes to the remote, a line at a time with
//! CR LF ends, and the remote's data comes to standard output.
//!
//! Option negotiation is minimal: the remote may echo and suppress go-ahead
//! (`DO` is sent for its `WILL ECHO` and `WILL SUPPRESS-GO-AHEAD`, `DONT`
//! for any other `WILL`), and every option the remote asks of us is
//! refused with `WONT`. Subnegotiations are skipped. A copy of this
//! program (`telnet --send`) carries standard input, so a prog can block
//! on each side; the session ends when the remote hangs up.

use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};

use purgatorio::abi::pctl;
use purgatorio::{Context, Error, dial};

const SELF: &str = "/bin/telnet.wasm";

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const ECHO: u8 = 1;
const SGA: u8 = 3;

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    if args.get(1).map(String::as_str) == Some("--send") {
        return send();
    }
    let [_, machine] = args.as_slice() else {
        return Err(Error::from("usage: telnet machine"));
    };
    let addr = match machine.split('!').count() {
        1 => format!("tcp!{machine}!23"),
        _ => machine.clone(),
    };
    let conn = dial::dial(&addr)?;
    let data = conn.data.ok_or_else(|| Error::from("no connection"))?;
    let fd = data.as_raw_fd();
    let sender = purgatorio::spawn(
        SELF,
        &[String::from("telnet"), String::from("--send")],
        pctl::NEWFD,
        &[(0, 0), (fd, 1), (2, 2)],
    )?;
    let got = receive(fd);
    // The remote is gone; so is the reason to read standard input.
    let _ = std::fs::write(format!("/prog/{sender}/ctl"), "kill");
    got
}

/// Copies the remote's data to standard output, answering options.
fn receive(fd: i32) -> Result<(), Error> {
    // SAFETY: `fd` stays open in `main` while this runs.
    let mut net = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    let mut st = State::Data;
    let mut cr = false;
    loop {
        let n = match net.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => return Err(Error::from(e)),
        };
        let mut text = Vec::with_capacity(n);
        let mut answer = Vec::new();
        for &b in &buf[..n] {
            st = match (st, b) {
                (State::Data, IAC) => State::Iac,
                (State::Data, _) => {
                    // CR LF and CR NUL are the network's end of line.
                    if cr && (b == b'\n' || b == 0) {
                        text.pop();
                    }
                    cr = b == b'\r';
                    if b != 0 {
                        text.push(b);
                    }
                    State::Data
                }
                (State::Iac, IAC) => {
                    text.push(IAC);
                    State::Data
                }
                (State::Iac, WILL | WONT | DO | DONT) => State::Opt(b),
                (State::Iac, SB) => State::Sub,
                (State::Iac, _) => State::Data,
                (State::Opt(verb), opt) => {
                    let reply = match verb {
                        WILL if opt == ECHO || opt == SGA => Some(DO),
                        WILL => Some(DONT),
                        DO => Some(WONT),
                        _ => None,
                    };
                    if let Some(r) = reply {
                        answer.extend_from_slice(&[IAC, r, opt]);
                    }
                    State::Data
                }
                (State::Sub, IAC) => State::SubIac,
                (State::Sub, _) => State::Sub,
                (State::SubIac, SE) => State::Data,
                (State::SubIac, _) => State::Sub,
            };
        }
        if !answer.is_empty() {
            net.write_all(&answer)?;
        }
        out.write_all(&text)?;
        out.flush()?;
    }
}

#[derive(Clone, Copy)]
enum State {
    Data,
    Iac,
    Opt(u8),
    Sub,
    SubIac,
}

/// `telnet --send`: standard input to fd 1, the connection.
fn send() -> Result<(), Error> {
    let mut stdin = std::io::stdin().lock();
    // SAFETY: fd 1 is the connection for the life of the prog.
    let mut net = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    let mut buf = [0u8; 4096];
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut out = Vec::with_capacity(n + 8);
        for &b in &buf[..n] {
            match b {
                b'\n' => out.extend_from_slice(b"\r\n"),
                IAC => out.extend_from_slice(&[IAC, IAC]),
                _ => out.push(b),
            }
        }
        net.write_all(&out)?;
    }
}
