//! The client: dial, authenticate, send the command, then serve this
//! prog's namespace to the server and carry the command's standard input
//! and output until its exit status arrives.
//!
//! One prog owns the connection's write side and waits on pipes only.
//! Three helpers feed those pipes: the command itself, run again as a pump
//! ([`crate::io::pump`]), copies the connection in, another copies
//! standard input in, and `export` serves the namespace on a pipe
//! whose other end this prog copies to and from STYX frames. The export
//! is the kernel's, so the server authenticates to it as any mounter does,
//! unless the server has already proved, on this connection, that it is
//! the host dialled: then the export asks no proof and runs in the clear
//! inside the encrypted connection (`docs/rcmd.md`, "Encryption").

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;

use purgatorio::abi::auth::{FAILED, NO_PQ, NO_SESSION, SERVER_NONE};
use purgatorio::abi::pctl::NEWFD;
use purgatorio::auth::fmt::proof::{OFFER, OFFER_PQ};
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
    /// `-S`: the server must prove itself with a key or certificate this
    /// namespace trusts as a server's, for the host dialled.
    pub srvauth: bool,
    /// `-k`: the authinfo to make the proof from.
    pub keyfile: Option<String>,
    /// `-x`: the directory served as `/n/client` (default `/`).
    pub export: Option<String>,
    /// `-e x25519mlkem768` (`-C` for cpu): use and require the hybrid
    /// X25519 + ML-KEM-768 key exchange (`docs/rcmd.md`, "Encryption").
    pub pq: bool,
}

/// The one cipher, the name `-e` and `-C` take for it.
pub const CIPHER: &str = "chacha20poly1305";
/// The default key exchange, also accepted by `-e` and `-C`.
pub const KX: &str = "x25519";
/// The hybrid post-quantum key exchange (`docs/auth.md`, "Post-quantum
/// options"), which the records then use: the name `mount -C` takes.
pub const KX_PQ: &str = "x25519mlkem768";

/// Checks an algorithm named with `-e` or `-C`, and says whether it asks
/// for the hybrid key exchange. An authenticated connection is always
/// encrypted, so `none` (or `clear`) goes only with `-A`, and the key
/// exchanges only without it.
pub fn check_alg(alg: &str, noauth: bool) -> Result<bool, Error> {
    match alg {
        CIPHER => Ok(false),
        KX if !noauth => Ok(false),
        KX_PQ if !noauth => Ok(true),
        "none" | "clear" if noauth => Ok(false),
        "none" | "clear" => Err(Error::from(
            "an authenticated connection is always encrypted; use -A for none",
        )),
        KX | KX_PQ => Err(Error::from("-A exchanges no keys")),
        _ => Err(Error(format!("unknown algorithm {alg}"))),
    }
}

fn text(d: &[u8]) -> String {
    String::from_utf8_lossy(d).into_owned()
}

/// Runs the opening exchange on `net`, sends `cmd`, and returns the link
/// the rest of the session goes over, and whether the server proved it is
/// `host`.
///
/// Without `-A` the server must ask for a proof and offer a session key:
/// a server that asks for none, or offers only a challenge, is refused
/// with `no session key offered` rather than spoken to in the clear.
/// With `-S` its OK must carry a proof of its own that this namespace
/// takes for `host` (`auth::check_server_host`).
fn hello(net: &mut Fd, o: &Opts, host: Option<&str>, cmd: &str) -> Result<(Link, bool), Error> {
    let user = auth::user();
    let magic = if o.pq { proto::MAGIC_PQ } else { proto::MAGIC };
    net.write_all(&frame(kind::HELLO, format!("{magic} {user}").as_bytes()))?;
    let challenge = match proto::read_frame(net)? {
        (kind::CHAL, d) => d,
        // A server that does not speak pgrstyx3 refuses the HELLO.
        (kind::ERR, d) if o.pq && d == EPROTO.as_bytes() => return Err(Error::from(NO_PQ)),
        (kind::ERR, d) => return Err(Error(text(&d))),
        _ => return Err(Error::from(EPROTO)),
    };
    let (proof, keys) = if o.noauth {
        (Vec::new(), None)
    } else {
        let mut random = [0u8; 32];
        auth::Rng::fill(&mut random);
        let client = if o.pq {
            // Only the long offer will do: anything shorter is a server
            // (or someone in between) leaving the ML-KEM key out.
            if challenge.len() != OFFER_PQ {
                return Err(Error::from(NO_PQ));
            }
            let mut m = [0u8; 32];
            auth::Rng::fill(&mut m);
            session::Client::new_pq(&challenge, random, m)
        } else {
            let offer: &[u8; OFFER] = challenge
                .as_slice()
                .try_into()
                .map_err(|_| Error::from(NO_SESSION))?;
            session::Client::new(offer, random)
        }
        .ok_or_else(|| Error::from(FAILED))?;
        let proof = auth::proof_for(&client.signed, &user, proto::ANAME, o.keyfile.as_deref())?;
        // A proof from an agent that does not make version 3 carries no
        // key: refuse rather than go on in the clear.
        let keys = client
            .keys(&user, proto::ANAME, &proof)
            .ok_or_else(|| Error::from(NO_SESSION))?;
        (proof, Some(keys))
    };
    net.write_all(&frame(kind::PROOF, &proof))?;
    let server = match proto::read_frame(net)? {
        (kind::OK, d) => d,
        (kind::ERR, d) => return Err(Error(text(&d))),
        _ => return Err(Error::from(EPROTO)),
    };
    // The server's proof, in OK. It signs this session's keys, which only
    // the real server shares, so it cannot be one from another session;
    // and it must be for the host dialled, so a server certified for
    // another name does not pass.
    let proved = match &keys {
        Some(k) => auth::check_server_host(k, &server, host),
        None => Err(Error::from(SERVER_NONE)),
    };
    if o.srvauth {
        proved.clone()?;
    }
    // Without a host to have proved, nothing skips the export's own check.
    let proved = proved.is_ok() && host.is_some();
    let mut link = keys.as_ref().map_or_else(Link::clear, Link::client);
    // A server that proved it is the host dialled is not asked again by
    // the export.
    let mut exec = vec![if o.noauth || proved { proto::NOAUTH } else { 0 }];
    exec.extend_from_slice(cmd.as_bytes());
    net.write_all(&link.wire(kind::EXEC, &exec).map_err(Error::from)?)?;
    Ok((link, proved))
}

/// Runs `cmd` at `host` and returns its exit status (empty for success).
pub fn run(o: &Opts, host: &str, cmd: &str) -> Result<String, Error> {
    if cmd.len() >= proto::MAX {
        return Err(Error::from("command too long"));
    }
    let addr = proto::mkaddr(host);
    let conn = dial::dial(&addr)?;
    let data = conn
        .data
        .as_ref()
        .ok_or_else(|| Error::from("no connection"))?
        .as_raw_fd();
    let mut net = Fd(data);
    let (mut link, proved) = hello(&mut net, o, dial::host(&addr).as_deref(), cmd)?;

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
    // The export asks the server for a proof of its own, and encrypts
    // again, unless the server has proved to this namespace that it is
    // the host dialled, on the encrypted connection that carries the
    // export: then the connection's keys already authenticate both ends
    // of it. A server that proved some other identity, or none, gets
    // `/n/client` only as a user this namespace trusts.
    if o.noauth || proved {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithms() {
        assert_eq!(check_alg(CIPHER, false).ok(), Some(false));
        assert_eq!(check_alg(CIPHER, true).ok(), Some(false));
        assert_eq!(check_alg(KX, false).ok(), Some(false));
        assert_eq!(check_alg(KX_PQ, false).ok(), Some(true));
        assert_eq!(check_alg("none", true).ok(), Some(false));
        for (alg, noauth) in [("none", false), (KX, true), (KX_PQ, true), ("x448", false)] {
            assert!(check_alg(alg, noauth).is_err(), "{alg} {noauth}");
        }
    }
}
