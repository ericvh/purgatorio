//! auxi/rstyxd [-A] [-k keyfile]: serve one `rcmd` or `cpu` client on file descriptor
//! 0, a connection `listen` accepted:
//!
//! ```text
//! listen 'tcp!*!rstyx' /bin/auxi/rstyxd.wasm &
//! ```
//!
//! The client proves who it is as a mounter does (`docs/auth.md`): the
//! keys and signers this namespace trusts are the ones an `export` here
//! would trust, and a client with no proof is refused when there are any.
//! `-A` asks for no proof. After a proof the connection is encrypted with
//! the keys the version 3 handshake agrees (`rstyx::link`); with `-A`, or
//! when this namespace trusts nothing, it stays in the clear, and a client
//! that did not ask for that with `-A` itself goes away. A version 1 or 2
//! proof, which carries no key, is refused. The client's export in turn
//! asks this side for
//! a proof when the client's namespace trusts anything; it is made from
//! the authinfo `keyfile`, or else from this user's default keys. The
//! client's command then runs under `sh -c`
//! in a copy of this namespace with the client's namespace mounted on
//! `/n/client`, and its exit status is sent back. The protocol is in
//! `docs/rcmd.md`.
//!
//! One prog owns the connection's write side and waits on pipes only.
//! Three helpers serve it: this command with `--pump` copies the
//! connection into a pipe, this
//! command again with `--feed` passes standard input on to the command
//! (and says how much it passed, which is the client's credit), and this
//! command again with `--session` mounts the client and runs the command.

mod session;

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

use purgatorio::abi::auth::NO_SESSION;
use purgatorio::abi::mode::OWRITE;
use purgatorio::abi::pctl::{FORKENV, FORKNS, NEWFD, NEWPGRP};
use purgatorio::arg::Arg;
use purgatorio::auth::fmt::keys::{parse_list, parse_list_lenient};
use purgatorio::auth::fmt::proof::{Policy, check};
use purgatorio::auth::fmt::session::Server as Offer;
use purgatorio::{Context, Error, auth};
use rstyx::io::{Children, Fd, kill, read_some, wait_readable};
use rstyx::link::Link;
use rstyx::proto::{self, EPROTO, frame, kind};

const SELF: &str = "/bin/auxi/rstyxd.wasm";
const USAGE: &str = "auxi/rstyxd [-A] [-k keyfile]";
/// How long output may still arrive once the command has exited: a prog
/// it left behind may hold the pipes open for ever.
const DRAIN_MS: i32 = 300;
/// How long the client has to close the connection after the status.
const LINGER_MS: i32 = 2000;

/// What this namespace trusts for `uname`, read where the kernel's export
/// reads it (`docs/auth.md`).
struct Trust {
    keys: Vec<[u8; 32]>,
    signers: Vec<[u8; 32]>,
    /// `None` when the list is there but does not read or parse: every
    /// certificate then fails.
    revoked: Option<Vec<[u8; 32]>>,
}

fn trust(uname: &str) -> Trust {
    let mut keys = Vec::new();
    let mut paths = vec![String::from("/lib/test/auth.key.pub")];
    if !uname.is_empty() && uname != "." && uname != ".." && !uname.contains('/') {
        paths.push(format!("/usr/{uname}/key.pub"));
    }
    for path in paths {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    let signers = std::fs::read("/lib/keys/signers")
        .map(|t| parse_list_lenient(&t))
        .unwrap_or_default();
    let revoked = match std::fs::read("/lib/keys/revoked") {
        Ok(t) => parse_list(&t),
        Err(_) if purgatorio::stat("/lib/keys/revoked").is_ok() => None,
        Err(_) => Some(Vec::new()),
    };
    Trust {
        keys,
        signers,
        revoked,
    }
}

/// The opening exchange. Returns the EXEC flags, the command and the link
/// the rest goes over, or the reason the client was refused (already sent
/// to it).
fn hello(net: &mut Fd, noauth: bool) -> Result<(u8, String, Link), String> {
    let refuse = |net: &mut Fd, why: &str| {
        let _ = net.write_all(&frame(kind::ERR, why.as_bytes()));
        String::from(why)
    };
    let (k, d) = proto::read_frame(net)?;
    let hello = String::from_utf8_lossy(&d).into_owned();
    let uname = match hello.split_once(' ') {
        Some((proto::MAGIC, u)) if k == kind::HELLO => u,
        _ => return Err(refuse(net, EPROTO)),
    };
    let t = trust(uname);
    let policy = Policy {
        keys: &t.keys,
        signers: &t.signers,
        revoked: t.revoked.as_deref(),
        now: auth::now(),
    };
    // The offer: a challenge and an ephemeral X25519 key.
    let offer = (!noauth && policy.requires_auth()).then(|| {
        let (mut challenge, mut random) = ([0u8; 32], [0u8; 32]);
        auth::Rng::fill(&mut challenge);
        auth::Rng::fill(&mut random);
        Offer::new(challenge, random)
    });
    let chal = offer.as_ref().map_or(&[][..], |o| &o.offer[..]);
    net.write_all(&frame(kind::CHAL, chal))
        .map_err(|_| String::from(proto::EHUNGUP))?;
    let (k, proof) = proto::read_frame(net)?;
    if k != kind::PROOF {
        return Err(refuse(net, EPROTO));
    }
    let mut link = Link::clear();
    if let Some(offer) = &offer {
        if proof.is_empty() {
            return Err(refuse(net, purgatorio::abi::auth::REQUIRED));
        }
        if let Err(e) = check(&policy, &offer.offer, uname, proto::ANAME, &proof) {
            return Err(refuse(net, e));
        }
        // Only a version 3 proof carries the client's key.
        let Some(keys) = offer.keys(uname, proto::ANAME, &proof) else {
            return Err(refuse(net, NO_SESSION));
        };
        link = Link::server(&keys);
    }
    net.write_all(&frame(kind::OK, &[]))
        .map_err(|_| String::from(proto::EHUNGUP))?;
    match link.read(net)? {
        (kind::EXEC, d) if !d.is_empty() => {
            Ok((d[0], String::from_utf8_lossy(&d[1..]).into_owned(), link))
        }
        // A refusal now would be in the clear on a sealed link; just go.
        _ => Err(String::from(EPROTO)),
    }
}

/// One of the command's output pipes and the credit left to send it.
struct Stream {
    fd: Option<OwnedFd>,
    kind: u8,
    credit: usize,
}

impl Stream {
    fn new(fd: OwnedFd, kind: u8) -> Stream {
        Stream {
            fd: Some(fd),
            kind,
            credit: proto::WINDOW,
        }
    }

    fn sendable(&self) -> Option<i32> {
        self.fd
            .as_ref()
            .filter(|_| self.credit > 0)
            .map(AsRawFd::as_raw_fd)
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    match args.get(1).map(String::as_str) {
        Some(rstyx::io::PUMP) => {
            rstyx::io::pump();
            return Ok(());
        }
        Some("--feed") => return session::feed(),
        Some("--session") => return session::run(&args[2..]),
        _ => {}
    }
    let mut arg = Arg::new(args, USAGE);
    let (mut noauth, mut keyfile) = (false, String::new());
    while let Some(c) = arg.opt() {
        match c {
            'A' => noauth = true,
            'k' | 'f' => keyfile = arg.earg()?,
            _ => return Err(arg.usage()),
        }
    }
    if !arg.argv().is_empty() {
        return Err(Error(format!("usage: {USAGE}")));
    }
    let mut net = Fd(0);
    let Ok((flags, cmd, mut link)) = hello(&mut net, noauth) else {
        // The client has the reason; a refused client is not an error here.
        return Ok(());
    };

    let mut children = Children::new();
    let [net_r, net_w] = purgatorio::pipe()?;
    let netpump = purgatorio::spawn(
        SELF,
        &["rstyxd", rstyx::io::PUMP].map(String::from),
        NEWFD,
        &[(0, 0), (net_w.as_raw_fd(), 1), (2, 2)],
    )?;
    drop(net_w);
    // Standard input: this prog -> feed pipe -> feeder -> the command.
    let [feed_r, feed_w] = purgatorio::pipe()?;
    let [in_r, in_w] = purgatorio::pipe()?;
    let [ack_r, ack_w] = purgatorio::pipe()?;
    let feeder = purgatorio::spawn(
        SELF,
        &["rstyxd", "--feed"].map(String::from),
        NEWFD,
        &[
            (feed_r.as_raw_fd(), 0),
            (in_w.as_raw_fd(), 1),
            (2, 2),
            (ack_w.as_raw_fd(), session::ACK),
        ],
    )?;
    drop((feed_r, in_w, ack_w));
    let [out_r, out_w] = purgatorio::pipe()?;
    let [err_r, err_w] = purgatorio::pipe()?;
    let [styx, styx_far] = purgatorio::pipe()?;
    let [st_r, st_w] = purgatorio::pipe()?;
    let argv = [
        String::from("rstyxd"),
        String::from("--session"),
        flags.to_string(),
        keyfile,
        cmd,
    ];
    // Its own process group, so that the command and all it starts can be
    // killed together; its own namespace, for the mount of the client.
    let session = purgatorio::spawn(
        SELF,
        &argv,
        NEWFD | FORKNS | FORKENV | NEWPGRP,
        &[
            (in_r.as_raw_fd(), 0),
            (out_w.as_raw_fd(), 1),
            (err_w.as_raw_fd(), 2),
            (styx_far.as_raw_fd(), session::STYX),
            (st_w.as_raw_fd(), session::STATUS),
        ],
    )?;
    drop((in_r, out_w, err_w, styx_far, st_w));
    // The session stays until it is killed, so this always names its group.
    let ctl = purgatorio::open(&format!("/prog/{session}/ctl"), OWRITE).map(File::from);

    let status = mux(
        &mut net,
        &mut link,
        &net_r,
        File::from(feed_w),
        ack_r,
        File::from(styx),
        st_r,
        [
            Stream::new(out_r, kind::OUT),
            Stream::new(err_r, kind::ERRS),
        ],
    );
    let sent = status.is_some_and(|s| {
        link.wire(kind::STATUS, s.as_bytes())
            .is_ok_and(|f| net.write_all(&f).is_ok())
    });
    if let Ok(mut ctl) = ctl {
        let _ = ctl.write_all(b"killgrp");
    }
    kill(session);
    kill(feeder);
    // The client closes first, once it has the status: closing here with
    // its last frames unread could reset the connection under the status.
    while sent
        && wait_readable(&[net_r.as_raw_fd()], LINGER_MS).is_ok_and(|w| w.is_some())
        && !read_some(net_r.as_raw_fd(), proto::MAX).is_empty()
    {}
    kill(netpump);
    children.reap(3, 2000);
    Ok(())
}

/// Carries the session until the command's status is known and its
/// output sent. `None` when the client went away first.
#[allow(clippy::too_many_arguments)]
fn mux(
    net: &mut Fd,
    link: &mut Link,
    net_r: &OwnedFd,
    feed: File,
    ack: OwnedFd,
    styx: File,
    st: OwnedFd,
    mut streams: [Stream; 2],
) -> Option<String> {
    let mut feed = Some(feed);
    let mut ack = Some(ack);
    let mut styx = Some(styx);
    let mut status: Option<String> = None;
    // A frame for the client, sealed when the link is; false when the
    // client has gone.
    let mut send =
        |link: &mut Link, k: u8, d: &[u8]| link.wire(k, d).is_ok_and(|f| net.write_all(&f).is_ok());
    #[derive(Clone, Copy, PartialEq)]
    enum Src {
        Net,
        Styx,
        Ack,
        Status,
        Stream(usize),
    }
    loop {
        if status.is_some() && streams.iter().all(|s| s.fd.is_none()) {
            return status;
        }
        let mut fds = vec![net_r.as_raw_fd()];
        let mut srcs = vec![Src::Net];
        if let Some(s) = styx.as_ref() {
            fds.push(s.as_raw_fd());
            srcs.push(Src::Styx);
        }
        if let Some(a) = ack.as_ref() {
            fds.push(a.as_raw_fd());
            srcs.push(Src::Ack);
        }
        if status.is_none() {
            fds.push(st.as_raw_fd());
            srcs.push(Src::Status);
        }
        for (i, s) in streams.iter().enumerate() {
            if let Some(fd) = s.sendable() {
                fds.push(fd);
                srcs.push(Src::Stream(i));
            }
        }
        // After the exit, output that could be sent but does not come is
        // not waited for. Output waiting for credit is: the client is
        // still reading.
        let idle = status.is_some() && streams.iter().all(|s| s.fd.is_none() || s.credit > 0);
        let which = match wait_readable(&fds, if idle { DRAIN_MS } else { -1 }) {
            Ok(Some(i)) => srcs[i],
            Ok(None) if status.is_some() => return status,
            _ => return None,
        };
        match which {
            Src::Net => {
                let bytes = read_some(fds[0], proto::MAX);
                if bytes.is_empty() {
                    return None;
                }
                link.push(&bytes);
                loop {
                    let (k, d) = match link.take() {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(_) => return None,
                    };
                    match k {
                        kind::STYX => {
                            if styx.as_mut().is_some_and(|s| s.write_all(&d).is_err()) {
                                styx = None;
                            }
                        }
                        // The window keeps this within the pipe's queue,
                        // so it does not block.
                        kind::IN if d.is_empty() => feed = None,
                        kind::IN => {
                            if feed.as_mut().is_some_and(|f| f.write_all(&d).is_err()) {
                                feed = None;
                            }
                        }
                        kind::WIN => match proto::parse_window(&d) {
                            Some((s, n)) if s == kind::OUT || s == kind::ERRS => {
                                let s = &mut streams[usize::from(s == kind::ERRS)];
                                s.credit = s.credit.saturating_add(n);
                            }
                            _ => return None,
                        },
                        _ => return None,
                    }
                }
            }
            Src::Styx => {
                let bytes = styx
                    .as_ref()
                    .map(|s| read_some(s.as_raw_fd(), proto::STYX_CHUNK))
                    .unwrap_or_default();
                if bytes.is_empty() {
                    styx = None;
                } else if !send(link, kind::STYX, &bytes) {
                    return None;
                }
            }
            Src::Ack => {
                let mut n = [0u8; 4];
                let fd = ack.as_ref().map_or(-1, AsRawFd::as_raw_fd);
                if Fd(fd).read_exact(&mut n).is_err() {
                    // The feeder has gone: no more input is wanted.
                    ack = None;
                    continue;
                }
                let w = proto::win_data(kind::IN, u32::from_le_bytes(n) as usize);
                if !send(link, kind::WIN, &w) {
                    return None;
                }
            }
            Src::Status => {
                let bytes = read_some(st.as_raw_fd(), proto::MAX);
                // The session writes `s` and the status; nothing at all
                // means it died first.
                status = Some(match bytes.split_first() {
                    Some((b's', s)) => String::from_utf8_lossy(s).into_owned(),
                    _ => String::from("rstyxd: session lost"),
                });
            }
            Src::Stream(i) => {
                let s = &mut streams[i];
                let fd = s.fd.as_ref().map_or(-1, AsRawFd::as_raw_fd);
                let bytes = read_some(fd, s.credit.min(proto::CHUNK));
                if bytes.is_empty() {
                    s.fd = None;
                } else {
                    s.credit -= bytes.len();
                    if !send(link, s.kind, &bytes) {
                        return None;
                    }
                }
            }
        }
    }
}
