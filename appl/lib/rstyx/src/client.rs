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

use purgatorio::abi::auth::{FAILED, NO_SESSION};
use purgatorio::abi::pctl::NEWFD;
use purgatorio::auth::fmt::proof::OFFER;
use purgatorio::auth::fmt::session;
use purgatorio::{Error, auth, dial};

use crate::io::{Children, Fd, kill, read_some, wait_readable};
use crate::link::Link;
use crate::proto::{self, EHUNGUP, EPROTO, frame, kind};

/// How a client connects.
#[derive(Default)]
pub struct Opts {
    /// The calling command's module, which is run again as each pump.
    pub module: &'static str,
    /// `-A`: prove nothing, ask no proof of the server, and speak in the
    /// clear. Without it the connection must be encrypted.
    pub noauth: bool,
    /// `-k`: the authinfo to make the proof from.
    pub keyfile: Option<String>,
    /// `-x`: the directory served as `/n/client` (default `/`).
    pub export: Option<String>,
}

/// The one cipher, the name `-e` and `-C` take for it.
pub const CIPHER: &str = "chacha20poly1305";

/// Checks an algorithm named with `-e` or `-C`. An authenticated
/// connection is always encrypted, so `none` (or `clear`) goes only with
/// `-A`.
pub fn check_alg(alg: &str, noauth: bool) -> Result<(), Error> {
    match alg {
        CIPHER => Ok(()),
        "none" | "clear" if noauth => Ok(()),
        "none" | "clear" => Err(Error::from(
            "an authenticated connection is always encrypted; use -A for none",
        )),
        _ => Err(Error(format!("unknown algorithm {alg}"))),
    }
}

fn text(d: &[u8]) -> String {
    String::from_utf8_lossy(d).into_owned()
}

/// Runs the opening exchange on `net`, sends `cmd`, and returns the link
/// the rest of the session goes over.
///
/// Without `-A` the server must ask for a proof and offer a session key:
/// a server that asks for none, or offers only a challenge, is refused
/// with `no session key offered` rather than spoken to in the clear.
fn hello(net: &mut Fd, o: &Opts, cmd: &str) -> Result<Link, Error> {
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
    let (proof, keys) = if o.noauth {
        (Vec::new(), None)
    } else {
        let offer: &[u8; OFFER] = challenge
            .as_slice()
            .try_into()
            .map_err(|_| Error::from(NO_SESSION))?;
        let mut random = [0u8; 32];
        auth::Rng::fill(&mut random);
        let client = session::Client::new(offer, random).ok_or_else(|| Error::from(FAILED))?;
        let proof = auth::proof_for(&client.signed, &user, proto::ANAME, o.keyfile.as_deref())?;
        // A proof from an agent that does not make version 3 carries no
        // key: refuse rather than go on in the clear.
        let keys = client
            .keys(&user, proto::ANAME, &proof)
            .ok_or_else(|| Error::from(NO_SESSION))?;
        (proof, Some(keys))
    };
    net.write_all(&frame(kind::PROOF, &proof))?;
    match proto::read_frame(net)? {
        (kind::OK, _) => {}
        (kind::ERR, d) => return Err(Error(text(&d))),
        _ => return Err(Error::from(EPROTO)),
    }
    let mut link = keys.as_ref().map_or_else(Link::clear, Link::client);
    let mut exec = vec![if o.noauth { proto::NOAUTH } else { 0 }];
    exec.extend_from_slice(cmd.as_bytes());
    net.write_all(&link.wire(kind::EXEC, &exec).map_err(Error::from)?)?;
    Ok(link)
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
    let mut link = hello(&mut net, o, cmd)?;

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
    argv.push(o.export.clone().unwrap_or_else(|| String::from("/")));
    purgatorio::spawn(
        "/bin/export.wasm",
        &argv,
        NEWFD,
        // No fd 2: an export whose pipe closes under a request says so,
        // and that is how every session ends.
        &[(exp_far.as_raw_fd(), 0)],
    )?;
    drop(exp_far);

    let result = mux(&mut net, &mut link, net_r, in_r, File::from(exp));
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
    link: &mut Link,
    net_r: std::os::fd::OwnedFd,
    in_r: std::os::fd::OwnedFd,
    exp: File,
) -> Result<String, Error> {
    let mut exp = Some(exp);
    let mut stdin = Some(in_r);
    let mut credit = proto::WINDOW;
    // A failed write means the server has closed its end. Its last frames,
    // the status among them, may still be on their way in: stop sending
    // and read on.
    let mut sending = true;
    let send = |net: &mut Fd, link: &mut Link, sending: &mut bool, k: u8, d: &[u8]| {
        let Ok(bytes) = link.wire(k, d) else {
            *sending = false;
            return;
        };
        if *sending && net.write_all(&bytes).is_err() {
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
            link.push(&bytes);
            while let Some((k, d)) = link.take()? {
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
                        send(
                            net,
                            link,
                            &mut sending,
                            kind::WIN,
                            &proto::win_data(k, d.len()),
                        );
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
                send(net, link, &mut sending, kind::STYX, &bytes);
            }
        } else if which.is_some() && which == in_at {
            let bytes = read_some(fds[which.unwrap_or(0)], credit.min(proto::CHUNK));
            credit -= bytes.len();
            send(net, link, &mut sending, kind::IN, &bytes);
            if bytes.is_empty() {
                stdin = None;
            }
        }
    }
}
