//! The client: dial, authenticate, send the command, then serve this
//! prog's namespace to the server and carry the command's standard input
//! and output until its exit status arrives.
//!
//! One prog owns the connection's write side and waits on pipes only.
//! Three helpers feed those pipes: the command itself, run again as a pump
//! ([`crate::io::pump`]), copies the connection in, another copies
//! standard input in, and `export` serves the namespace on a pipe
//! whose other end this prog copies to and from STYX frames. The export
//! is the kernel's, so the server authenticates to it as any mounter does.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;

use purgatorio::abi::pctl::NEWFD;
use purgatorio::{Error, auth, dial};

use crate::io::{Children, Fd, kill, read_some, wait_readable};
use crate::proto::{self, Decoder, EHUNGUP, EPROTO, frame, kind};

/// How a client connects.
#[derive(Default)]
pub struct Opts {
    /// The calling command's module, which is run again as each pump.
    pub module: &'static str,
    /// `-A`: prove nothing, and ask no proof of the server.
    pub noauth: bool,
    /// `-k`: the authinfo to make the proof from.
    pub keyfile: Option<String>,
}

fn text(d: &[u8]) -> String {
    String::from_utf8_lossy(d).into_owned()
}

/// Runs the opening exchange on `net` and sends `cmd`.
fn hello(net: &mut Fd, o: &Opts, cmd: &str) -> Result<(), Error> {
    let user = auth::user();
    net.write_all(&frame(
        kind::HELLO,
        format!("{} {user}", proto::MAGIC).as_bytes(),
    ))?;
    let challenge = match proto::read_frame(net)? {
        (kind::CHAL, d) => d,
        (kind::ERR, d) => return Err(Error(text(&d))),
        _ => return Err(Error::from(EPROTO)),
    };
    let proof = if challenge.is_empty() || o.noauth {
        Vec::new()
    } else {
        auth::proof_for(&challenge, &user, proto::ANAME, o.keyfile.as_deref())?
    };
    net.write_all(&frame(kind::PROOF, &proof))?;
    match proto::read_frame(net)? {
        (kind::OK, _) => {}
        (kind::ERR, d) => return Err(Error(text(&d))),
        _ => return Err(Error::from(EPROTO)),
    }
    let mut exec = vec![if o.noauth { proto::NOAUTH } else { 0 }];
    exec.extend_from_slice(cmd.as_bytes());
    net.write_all(&frame(kind::EXEC, &exec))?;
    Ok(())
}

/// Runs `cmd` at `host` and returns its exit status (empty for success).
pub fn run(o: &Opts, host: &str, cmd: &str) -> Result<String, Error> {
    if cmd.len() >= proto::MAX {
        return Err(Error::from("command too long"));
    }
    let conn = dial::dial(&proto::mkaddr(host))?;
    let data = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("no connection"))?
        .as_raw_fd();
    let mut net = Fd(data);
    hello(&mut net, o, cmd)?;

    let mut children = Children::new();
    let pump = [String::from("pump"), String::from(crate::io::PUMP)];
    let [net_r, net_w] = purgatorio::pipe()?;
    let netpump = purgatorio::spawn(
        o.module,
        &pump,
        NEWFD,
        &[(data, 0), (net_w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(net_w);
    let [in_r, in_w] = purgatorio::pipe()?;
    let inpump = purgatorio::spawn(
        o.module,
        &pump,
        NEWFD,
        &[(0, 0), (in_w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(in_w);
    let [exp, exp_far] = purgatorio::pipe()?;
    let mut argv = vec![String::from("export")];
    if o.noauth {
        argv.push(String::from("-A"));
    }
    argv.push(String::from("/"));
    purgatorio::spawn(
        "/bin/export.wasm",
        &argv,
        NEWFD,
        // No fd 2: an export whose pipe closes under a request says so,
        // and that is how every session ends.
        &[(exp_far.as_raw_fd(), 0)],
    )?;
    drop(exp_far);

    let result = mux(&mut net, net_r, in_r, File::from(exp));
    // The export ends when its pipe closed (in `mux`); the pumps are
    // blocked in reads that only a kill ends.
    kill(netpump);
    kill(inpump);
    drop(conn);
    children.reap(3, 2000);
    result
}

fn mux(
    net: &mut Fd,
    net_r: std::os::fd::OwnedFd,
    in_r: std::os::fd::OwnedFd,
    exp: File,
) -> Result<String, Error> {
    let mut exp = Some(exp);
    let mut stdin = Some(in_r);
    let mut credit = proto::WINDOW;
    let mut dec = Decoder::default();
    // A failed write means the server has closed its end. Its last frames,
    // the status among them, may still be on their way in: stop sending
    // and read on.
    let mut sending = true;
    let send = |net: &mut Fd, sending: &mut bool, bytes: &[u8]| {
        if *sending && net.write_all(bytes).is_err() {
            *sending = false;
        }
    };
    loop {
        let mut fds = vec![net_r.as_raw_fd()];
        let exp_at = exp.as_ref().filter(|_| sending).map(|e| {
            fds.push(e.as_raw_fd());
            fds.len() - 1
        });
        let in_at = stdin.as_ref().filter(|_| sending && credit > 0).map(|s| {
            fds.push(s.as_raw_fd());
            fds.len() - 1
        });
        let which = wait_readable(&fds, -1)?;
        if which == Some(0) {
            let bytes = read_some(fds[0], proto::MAX);
            if bytes.is_empty() {
                return Err(Error::from(EHUNGUP));
            }
            dec.push(&bytes);
            while let Some((k, d)) = dec.take()? {
                match k {
                    kind::STYX => {
                        if exp.as_mut().is_some_and(|e| e.write_all(&d).is_err()) {
                            exp = None;
                        }
                    }
                    kind::OUT | kind::ERRS => {
                        // A closed output loses the bytes, as it would
                        // lose a local command's.
                        let _ = Fd(if k == kind::OUT { 1 } else { 2 }).write_all(&d);
                        send(net, &mut sending, &proto::window(k, d.len()));
                    }
                    kind::WIN => match proto::parse_window(&d) {
                        Some((kind::IN, n)) => credit = credit.saturating_add(n),
                        _ => return Err(Error::from(EPROTO)),
                    },
                    kind::STATUS => return Ok(text(&d)),
                    _ => return Err(Error::from(EPROTO)),
                }
            }
        } else if which.is_some() && which == exp_at {
            let bytes = read_some(fds[which.unwrap_or(0)], proto::STYX_CHUNK);
            if bytes.is_empty() {
                exp = None;
            } else {
                send(net, &mut sending, &frame(kind::STYX, &bytes));
            }
        } else if which.is_some() && which == in_at {
            let bytes = read_some(fds[which.unwrap_or(0)], credit.min(proto::CHUNK));
            credit -= bytes.len();
            send(net, &mut sending, &frame(kind::IN, &bytes));
            if bytes.is_empty() {
                stdin = None;
            }
        }
    }
}
