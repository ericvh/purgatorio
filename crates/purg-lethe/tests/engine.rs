//! The server engine and the client together over an in-memory transport:
//! the exchange, each request, renewal, replicas, narrowing, and every
//! refusal `docs/lethe.md` names.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use purg_auth::cert::Cert;
use purg_lethe::client::{self, CallError, Client, Creds, Transport};
use purg_lethe::msg::{Frame, Msg, Op, Owned};
use purg_lethe::server::{Config, Env, Event, Server, Tree, Trust};
use purg_lethe::{NONCE, error, rights};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

const USER: [u8; 32] = [1; 32];
const SIGNER: [u8; 32] = [2; 32];
const T0: u64 = 1_700_000_000;

/// Mode, owner, content (none for a directory) and version.
type Entry = (u32, String, Option<Vec<u8>>, u32);

/// A tree in memory, by path.
#[derive(Default)]
struct Mem {
    files: RefCell<BTreeMap<String, Entry>>,
}

impl Mem {
    fn add(&self, path: &str, mode: u32, owner: &str, data: Option<&[u8]>) {
        self.files.borrow_mut().insert(
            path.to_string(),
            (mode, owner.to_string(), data.map(<[u8]>::to_vec), 1),
        );
    }

    fn change(&self, path: &str, data: &[u8]) {
        let mut f = self.files.borrow_mut();
        let e = f.get_mut(path).unwrap();
        e.2 = Some(data.to_vec());
        e.3 += 1;
    }

    fn dir(&self, path: &str) -> Option<Dir> {
        let f = self.files.borrow();
        let (mode, owner, data, vers) = f.get(path)?;
        let name = path
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("/");
        Some(Dir {
            qid: Qid {
                path: path.len() as u64,
                vers: *vers,
                qtype: if data.is_none() { QTDIR } else { QTFILE },
            },
            mode: *mode | if data.is_none() { DMDIR } else { 0 },
            length: data.as_ref().map_or(0, |d| d.len() as u64),
            name: name.to_string(),
            uid: owner.clone(),
            gid: owner.clone(),
            muid: owner.clone(),
            ..Dir::default()
        })
    }
}

impl Tree for Mem {
    fn stat(&self, path: &str) -> Result<Dir, String> {
        self.dir(path)
            .ok_or_else(|| String::from("file does not exist"))
    }

    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(Qid, usize), String> {
        let d = self.stat(path)?;
        let f = self.files.borrow();
        let data = f[path].2.as_ref().unwrap();
        let off = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok((d.qid, n))
    }

    fn list(&self, path: &str, after: &[u8], limit: usize) -> Result<(Qid, Vec<Dir>), String> {
        let d = self.stat(path)?;
        let prefix = if path == "/" {
            String::from("/")
        } else {
            format!("{path}/")
        };
        let names: Vec<String> = self
            .files
            .borrow()
            .keys()
            .filter(|k| k.starts_with(&prefix) && k.len() > prefix.len())
            .filter(|k| !k[prefix.len()..].contains('/'))
            .cloned()
            .collect();
        let mut all: Vec<Dir> = names.iter().filter_map(|n| self.dir(n)).collect();
        all.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        all.retain(|e| e.name.as_bytes() > after);
        all.truncate(limit);
        Ok((d.qid, all))
    }
}

fn tree() -> Mem {
    let m = Mem::default();
    m.add("/", 0o755, "sys", None);
    m.add("/hello", 0o644, "sys", Some(b"hello, lethe\n"));
    m.add("/secret", 0o600, "sys", Some(b"no"));
    m.add("/mine", 0o600, "glenda", Some(b"yes"));
    m.add("/lib", 0o755, "sys", None);
    m.add("/lib/a", 0o644, "sys", Some(b"a"));
    m.add("/locked", 0o700, "sys", None);
    m.add("/locked/x", 0o644, "sys", Some(b"x"));
    m.add("/many", 0o755, "sys", None);
    for i in 0..200 {
        m.add(&format!("/many/f{i:03}"), 0o644, "sys", Some(b""));
    }
    m
}

/// A server and a tree behind a transport, with a clock the test moves.
struct Rig<'a> {
    server: &'a Server,
    tree: &'a Mem,
    trust: Trust,
    now: &'a Cell<u64>,
    events: Vec<Event>,
    /// Every frame the client sent, for replays.
    sent: Vec<Vec<u8>>,
    /// Changes the next frame on the way when set.
    tamper: Option<usize>,
}

fn random(buf: &mut [u8]) {
    thread_local!(static N: Cell<u64> = const { Cell::new(1) });
    for b in buf {
        N.with(|n| {
            let x = n
                .get()
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            n.set(x);
            *b = (x >> 33) as u8;
        });
    }
}

impl Rig<'_> {
    fn send(&mut self, frame: &[u8]) -> Vec<u8> {
        let trust = self.trust.clone();
        let mut t = move |_: &str| trust.clone();
        let mut env = Env {
            now: self.now.get(),
            random: &mut random,
            trust: &mut t,
        };
        let (reply, ev) = self.server.handle(self.tree, &mut env, frame);
        self.events.push(ev);
        reply
    }
}

impl Transport for Rig<'_> {
    fn exchange(&mut self, frame: &[u8]) -> Result<Vec<u8>, String> {
        self.sent.push(frame.to_vec());
        let mut f = frame.to_vec();
        if let Some(i) = self.tamper.take() {
            f[i] ^= 1;
        }
        Ok(self.send(&f))
    }
}

fn direct() -> Trust {
    Trust {
        keys: vec![purg_auth::public_key(&USER)],
        signers: vec![],
        revoked: Some(vec![]),
    }
}

fn creds(cert: Option<Cert>) -> Creds {
    Creds {
        user: String::from("glenda"),
        seed: USER,
        cert,
    }
}

fn cert(expires: u64) -> Cert {
    let mut c = Cert::new("glenda", purg_auth::public_key(&USER), expires).unwrap();
    c.sign(&SIGNER).unwrap();
    c
}

fn call(c: &mut Client, rig: &mut Rig<'_>, op: &Op<'_>) -> Result<Owned, CallError> {
    let now = rig.now;
    c.call(rig, op, &|| now.get(), &mut random)
}

fn read(
    c: &mut Client,
    rig: &mut Rig<'_>,
    path: &str,
    have: Option<u32>,
) -> Result<Owned, CallError> {
    call(
        c,
        rig,
        &Op::Read {
            path,
            offset: 0,
            count: 8192,
            have,
        },
    )
}

fn rig<'a>(server: &'a Server, tree: &'a Mem, now: &'a Cell<u64>, trust: Trust) -> Rig<'a> {
    Rig {
        server,
        tree,
        trust,
        now,
        events: vec![],
        sent: vec![],
        tamper: None,
    }
}

fn remote(e: &str) -> Result<Owned, CallError> {
    Err(CallError::Remote(String::from(e)))
}

#[test]
fn stat_read_readdir() {
    let server = Server::new(Config::default(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ, "/");
    let Ok(Owned::Stat(d)) = call(&mut c, &mut r, &Op::Stat { path: "/hello" }) else {
        panic!()
    };
    assert_eq!((d.name.as_str(), d.length), ("hello", 13));
    assert!(matches!(&r.events[0], Event::Issued { uname, .. } if uname == "glenda"));
    let Ok(Owned::Read {
        qid,
        unchanged,
        data,
    }) = read(&mut c, &mut r, "/hello", None)
    else {
        panic!()
    };
    assert_eq!(
        (data.as_slice(), unchanged),
        (&b"hello, lethe\n"[..], false)
    );
    // Held at this version: nothing comes back but the qid.
    let Ok(Owned::Read {
        unchanged, data, ..
    }) = read(&mut c, &mut r, "/hello", Some(qid.vers))
    else {
        panic!()
    };
    assert!(unchanged && data.is_empty());
    // Changed underneath: the new version and the new data.
    t.change("/hello", b"changed\n");
    let Ok(Owned::Read {
        qid: q2,
        unchanged,
        data,
    }) = read(&mut c, &mut r, "/hello", Some(qid.vers))
    else {
        panic!()
    };
    assert!(!unchanged && q2.vers != qid.vers);
    assert_eq!(data, b"changed\n");
    // A range.
    let Ok(Owned::Read { data, .. }) = call(
        &mut c,
        &mut r,
        &Op::Read {
            path: "/hello",
            offset: 3,
            count: 2,
            have: None,
        },
    ) else {
        panic!()
    };
    assert_eq!(data, b"ng");
    // Paging: 200 entries in pages of at most 1000 bytes.
    let mut cookie = Vec::new();
    let mut names = Vec::new();
    let mut pages = 0;
    loop {
        let Ok(Owned::Readdir {
            cookie: next,
            entries,
            ..
        }) = call(
            &mut c,
            &mut r,
            &Op::Readdir {
                path: "/many",
                cookie: &cookie,
                count: 1000,
            },
        )
        else {
            panic!()
        };
        pages += 1;
        names.extend(entries.into_iter().map(|d| d.name));
        if next.is_empty() {
            break;
        }
        cookie = next;
    }
    assert!(pages > 5, "{pages}");
    assert_eq!(names.len(), 200);
    assert!(names.windows(2).all(|w| w[0] < w[1]));
    // One ticket for all of it.
    assert_eq!(c.tickets, 1);
}

#[test]
fn file_errors_are_sealed() {
    let server = Server::new(Config::default(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert_eq!(
        call(&mut c, &mut r, &Op::Stat { path: "/nope" }),
        remote("file does not exist")
    );
    // Others' bits on a file owned by someone else; the owner's for one's own.
    assert_eq!(
        read(&mut c, &mut r, "/secret", None),
        remote("permission denied")
    );
    assert!(read(&mut c, &mut r, "/mine", None).is_ok());
    // No search permission on a directory above.
    assert_eq!(
        call(&mut c, &mut r, &Op::Stat { path: "/locked/x" }),
        remote("permission denied")
    );
    assert_eq!(
        read(&mut c, &mut r, "/lib", None),
        remote("file is a directory")
    );
    assert_eq!(
        call(
            &mut c,
            &mut r,
            &Op::Readdir {
                path: "/hello",
                cookie: b"",
                count: 100
            }
        ),
        remote("not a directory")
    );
    assert_eq!(
        call(&mut c, &mut r, &Op::Stat { path: "/a/../b" }),
        remote("file name syntax")
    );
    assert_eq!(
        call(
            &mut c,
            &mut r,
            &Op::Readdir {
                path: "/many",
                cookie: b"",
                count: 10
            }
        ),
        remote("stat buffer too small")
    );
    // A missing file's name is never in the clear: the error is sealed.
    let last = r.sent.last().unwrap().clone();
    let reply = r.send(&last);
    assert!(!String::from_utf8_lossy(&reply).contains("does not exist"));
}

#[test]
fn certificates_and_refusals() {
    let server = Server::new(Config::default(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let signers = Trust {
        keys: vec![],
        signers: vec![purg_auth::public_key(&SIGNER)],
        revoked: Some(vec![]),
    };
    let mut r = rig(&server, &t, &now, signers.clone());
    let mut c = Client::new(creds(Some(cert(T0 + 100))), rights::READ, "/");
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    // A direct key the server does not hold.
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("authentication failed")
    );
    // Expired, untrusted, revoked, for someone else.
    let mut c = Client::new(creds(Some(cert(T0))), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("certificate expired")
    );
    let mut other = Cert::new("glenda", purg_auth::public_key(&USER), 0).unwrap();
    other.sign(&[3; 32]).unwrap();
    let mut c = Client::new(creds(Some(other)), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("untrusted signer")
    );
    r.trust.revoked = Some(vec![purg_auth::public_key(&USER)]);
    let mut c = Client::new(creds(Some(cert(0))), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("certificate revoked")
    );
    r.trust = signers;
    let mut someone = creds(Some(cert(0)));
    someone.user = String::from("bob");
    let mut c = Client::new(someone, rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("certificate name mismatch")
    );
    // A server trusting nobody refuses everybody, as an export does.
    r.trust = Trust::default();
    let mut c = Client::new(creds(Some(cert(0))), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("server trusts no keys")
    );
    // A changed Tauth: the signature no longer verifies.
    r.trust = direct();
    let mut c = Client::new(creds(None), rights::READ, "/");
    r.tamper = Some(60);
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("authentication failed")
    );
}

#[test]
fn renewal_window_tampering_and_replicas() {
    let cfg = Config {
        lifetime: 30,
        window: 10,
        allow: rights::READ,
    };
    let server = Server::new(cfg.clone(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    // Expired: the client renews before sending.
    now.set(T0 + 40);
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    assert_eq!(c.tickets, 2);
    // A ticket the server says expired is renewed and the request resent.
    let mut stale = Client::new(creds(None), rights::READ, "/");
    stale.session = c.session.clone();
    now.set(T0 + 40 + 31);
    let ticket = stale.session.as_ref().unwrap().clone();
    let frame = ticket
        .request(5, T0 + 40, &Op::Stat { path: "/" }, [7; NONCE])
        .0;
    assert_eq!(
        r.send(&frame),
        purg_lethe::msg::rerror(5, error::TICKET_EXPIRED)
    );
    // The client's clock is behind, so it thinks the ticket still good.
    let behind = Cell::new(T0 + 40);
    let tickets = stale.tickets;
    assert!(
        stale
            .call(
                &mut r,
                &Op::Stat { path: "/" },
                &|| behind.get(),
                &mut random
            )
            .is_ok()
    );
    assert_eq!(stale.tickets, tickets + 1);

    // A request replayed after the window is refused; inside it, it is
    // answered again (reads are idempotent) under a fresh nonce.
    now.set(T0 + 100);
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    let req = r.sent.last().unwrap().clone();
    let a = r.send(&req);
    let b = r.send(&req);
    assert_ne!(a, b);
    assert!(matches!(Msg::decode(&a).unwrap().frame, Frame::Rreq { .. }));
    now.set(T0 + 111);
    assert_eq!(
        r.send(&req),
        purg_lethe::msg::rerror(purg_lethe::msg::frame_id(&req), error::REQUEST_EXPIRED)
    );
    // Every changed byte of a request is refused in the clear.
    now.set(T0 + 100);
    for i in [4, 5, 6, 12, 20, 60, req.len() - 30, req.len() - 1] {
        let mut bad = req.clone();
        bad[i] ^= 0x01;
        let m = r.send(&bad);
        assert!(
            matches!(Msg::decode(&m).unwrap().frame, Frame::Rerror { .. }),
            "byte {i}"
        );
    }
    let mut bad = req.clone();
    let n = bad.len();
    bad[n - 1] ^= 1;
    let reply = r.send(&bad);
    assert_eq!(
        Msg::decode(&reply).unwrap().frame,
        Frame::Rerror { ename: error::MAC }
    );

    // A replica holding the same ticket key answers the same ticket; one
    // with another key says so, and the client authenticates there.
    let replica = Server::new(cfg.clone(), [9; 32]);
    let mut r2 = rig(&replica, &t, &now, direct());
    let tickets = c.tickets;
    assert!(read(&mut c, &mut r2, "/hello", None).is_ok());
    assert_eq!(c.tickets, tickets);
    let other = Server::new(cfg, [8; 32]);
    let mut r3 = rig(&other, &t, &now, direct());
    assert!(read(&mut c, &mut r3, "/hello", None).is_ok());
    assert_eq!(c.tickets, tickets + 1);
    assert!(r3.events.contains(&Event::Refused(error::TICKET_KEY)));
}

#[test]
fn narrowing() {
    let server = Server::new(Config::default(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ | rights::WRITE, "/lib");
    assert!(read(&mut c, &mut r, "/lib/a", None).is_ok());
    let s = c.session.as_ref().unwrap();
    // Write was asked for and not granted; the prefix was.
    assert_eq!((s.rights, s.prefix.as_str()), (rights::READ, "/lib"));
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("permission denied")
    );
    assert_eq!(
        call(&mut c, &mut r, &Op::Stat { path: "/" }),
        remote("permission denied")
    );
    assert_eq!(
        call(&mut c, &mut r, &Op::Stat { path: "/libx" }),
        remote("permission denied")
    );
    // Nothing grantable asked for.
    let mut c = Client::new(creds(None), rights::WRITE, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("permission denied")
    );
}

/// A reserved write op is refused as a read-only tree refuses writes, and
/// an unknown op is not implemented; both sealed.
#[test]
fn reserved_ops() {
    let server = Server::new(Config::default(), [9; 32]);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    let s = c.session.clone().unwrap();
    for (code, want) in [
        (0x20u8, "permission denied"),
        (0x40, "not implemented"),
        (0x16, "not implemented"),
    ] {
        let mut plain = T0.to_le_bytes().to_vec();
        plain.push(code);
        let (frame, pending) = s.seal(3, &plain, [4; NONCE]);
        let reply = r.send(&frame);
        assert_eq!(
            s.reply(&pending, &reply),
            Err(client::Failure::Sealed(String::from(want))),
            "{code:#x}"
        );
    }
    // A request that does not decode at all, and one with no time.
    let (frame, pending) = s.seal(3, &T0.to_le_bytes(), [4; NONCE]);
    let reply = r.send(&frame);
    assert_eq!(
        s.reply(&pending, &reply),
        Err(client::Failure::Sealed(String::from(error::MESSAGE)))
    );
    let (frame, pending) = s.seal(3, &[1, 2], [4; NONCE]);
    let reply = r.send(&frame);
    assert_eq!(
        s.reply(&pending, &reply),
        Err(client::Failure::Clear(String::from(error::REQUEST_EXPIRED)))
    );
}

fn clear(rig: &mut Rig<'_>, op: &Op<'_>) -> Result<Owned, CallError> {
    client::clear_call(rig, 7, op)
}

fn clear_read(rig: &mut Rig<'_>, path: &str, have: Option<u32>) -> Result<Owned, CallError> {
    clear(
        rig,
        &Op::Read {
            path,
            offset: 0,
            count: 8192,
            have,
        },
    )
}

/// A server with `-A` and no trust: clear requests are answered as
/// `none`, and nobody gets a ticket.
#[test]
fn clear_requests() {
    let server = Server::new(Config::default(), [9; 32]).with_clear(true);
    let t = tree();
    t.add(
        "/nones",
        0o600,
        "none",
        Some(b"owned by a user called none"),
    );
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, Trust::default());
    let Ok(Owned::Stat(d)) = clear(&mut r, &Op::Stat { path: "/hello" }) else {
        panic!()
    };
    assert_eq!((d.name.as_str(), d.length), ("hello", 13));
    assert_eq!(
        r.events[0],
        Event::Answered {
            uname: String::from("none"),
            op: "stat",
            path: String::from("/hello"),
            error: None,
        }
    );
    // The request and the reply are plain bytes: no ticket, no seal.
    let sent = r.sent.last().unwrap();
    assert_eq!(sent[5], purg_lethe::kind::TCLEAR);
    assert_eq!(&sent[10..], Op::Stat { path: "/hello" }.encode());
    let Ok(Owned::Read { qid, data, .. }) = clear_read(&mut r, "/hello", None) else {
        panic!()
    };
    assert_eq!(data, b"hello, lethe\n");
    let last = r.sent.last().unwrap().clone();
    let reply = r.send(&last);
    assert_eq!(reply[5], purg_lethe::kind::RCLEAR);
    assert!(String::from_utf8_lossy(&reply).contains("hello, lethe"));
    // Conditional reads and versions, as in the sealed form.
    let Ok(Owned::Read {
        unchanged, data, ..
    }) = clear_read(&mut r, "/hello", Some(qid.vers))
    else {
        panic!()
    };
    assert!(unchanged && data.is_empty());
    t.change("/hello", b"changed\n");
    let Ok(Owned::Read {
        unchanged, data, ..
    }) = clear_read(&mut r, "/hello", Some(qid.vers))
    else {
        panic!()
    };
    assert!(!unchanged);
    assert_eq!(data, b"changed\n");
    // Paging.
    let mut cookie = Vec::new();
    let mut names = Vec::new();
    loop {
        let op = Op::Readdir {
            path: "/many",
            cookie: &cookie,
            count: 1000,
        };
        let Ok(Owned::Readdir {
            cookie: next,
            entries,
            ..
        }) = clear(&mut r, &op)
        else {
            panic!()
        };
        names.extend(entries.into_iter().map(|d| d.name));
        if next.is_empty() {
            break;
        }
        cookie = next;
    }
    assert_eq!(names.len(), 200);
    assert_eq!(names[199], "f199");
    // `none` gets the other bits, never an owner's, whatever the owner
    // is called; and needs search on every directory above.
    for p in ["/secret", "/mine", "/nones"] {
        assert_eq!(clear_read(&mut r, p, None), remote("permission denied"));
    }
    assert_eq!(
        clear(&mut r, &Op::Stat { path: "/locked/x" }),
        remote("permission denied")
    );
    assert_eq!(
        clear(
            &mut r,
            &Op::Readdir {
                path: "/locked",
                cookie: b"",
                count: 100
            }
        ),
        remote("permission denied")
    );
    // The same file errors, and the same path rules.
    assert_eq!(
        clear(&mut r, &Op::Stat { path: "/nope" }),
        remote("file does not exist")
    );
    assert_eq!(
        clear_read(&mut r, "/lib", None),
        remote("file is a directory")
    );
    for p in ["/a/../b", "/..", "lib", "/lib/", ""] {
        assert_eq!(
            clear(&mut r, &Op::Stat { path: p }),
            remote("file name syntax"),
            "{p:?}"
        );
    }
    // Reserved ops and bodies that do not decode.
    for (body, want) in [
        (&[0x20u8][..], "permission denied"),
        (&[0x40], "not implemented"),
        (&[0x16], "not implemented"),
        (&[], error::MESSAGE),
        (&[0x10, 9], error::MESSAGE),
    ] {
        let frame = Msg {
            id: 3,
            frame: Frame::Tclear { request: body },
        }
        .encode();
        let reply = r.send(&frame);
        let Frame::Rclear { reply } = Msg::decode(&reply).unwrap().frame else {
            panic!("{body:?}")
        };
        assert_eq!(
            purg_lethe::msg::Reply::decode(reply),
            Ok(purg_lethe::msg::Reply::Error(want)),
            "{body:?}"
        );
    }
    // A server that allows no reads allows none in the clear either.
    let cfg = Config {
        allow: 0,
        ..Config::default()
    };
    let closed = Server::new(cfg, [9; 32]).with_clear(true);
    let mut r = rig(&closed, &t, &now, Trust::default());
    assert_eq!(
        clear_read(&mut r, "/hello", None),
        remote("permission denied")
    );
}

/// Neither end falls back: each mismatch is an error with a name.
#[test]
fn clear_and_sealed_do_not_mix() {
    let t = tree();
    let now = Cell::new(T0);
    // A clear request to a server without `-A`, whatever it trusts.
    let sealed_only = Server::new(Config::default(), [9; 32]);
    for trust in [direct(), Trust::default()] {
        let mut r = rig(&sealed_only, &t, &now, trust);
        assert_eq!(
            clear_read(&mut r, "/hello", None),
            remote("authentication required")
        );
        assert_eq!(error::AUTH_REQUIRED, "authentication required");
        assert_eq!(r.events, [Event::Refused("authentication required")]);
        // Refused before the request is looked at.
        let junk = Msg {
            id: 1,
            frame: Frame::Tclear { request: &[0xff] },
        }
        .encode();
        let reply = r.send(&junk);
        assert!(matches!(
            Msg::decode(&reply).unwrap().frame,
            Frame::Rerror {
                ename: "authentication required"
            }
        ));
    }
    // A sealed client and a server with `-A` that trusts no key: no
    // ticket, and the client does not go on in the clear.
    let clear_only = Server::new(Config::default(), [9; 32]).with_clear(true);
    let mut r = rig(&clear_only, &t, &now, Trust::default());
    let mut c = Client::new(creds(None), rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("authentication not required")
    );
    assert_eq!(error::AUTH_NOT_REQUIRED, "authentication not required");
    assert_eq!(r.sent.len(), 1);
    assert_eq!(r.sent[0][5], purg_lethe::kind::TAUTH);
    assert!(c.session.is_none());
    // A clear client takes no sealed reply, a sealed client no clear one.
    let mut r = rig(&clear_only, &t, &now, direct());
    assert!(read(&mut c, &mut r, "/hello", None).is_ok());
    let s = c.session.clone().unwrap();
    let op = Op::Stat { path: "/hello" };
    let (sealed, sealed_pending) = s.request(7, T0, &op, [4; NONCE]);
    let (plain, plain_pending) = client::clear(7, &op);
    let sealed_reply = r.send(&sealed);
    let plain_reply = r.send(&plain);
    assert!(s.reply(&sealed_pending, &sealed_reply).is_ok());
    assert!(plain_pending.reply(&plain_reply).is_ok());
    assert_eq!(
        plain_pending.reply(&sealed_reply),
        Err(client::Failure::Bad(error::MESSAGE))
    );
    assert_eq!(
        s.reply(&sealed_pending, &plain_reply),
        Err(client::Failure::Bad(error::MESSAGE))
    );
    // A reply for another request.
    let (_, other) = client::clear(8, &op);
    assert_eq!(
        other.reply(&plain_reply),
        Err(client::Failure::Bad(error::MESSAGE))
    );
}

/// `-A` with trust: both forms are answered, each as who it is.
#[test]
fn mixed_mode() {
    let server = Server::new(Config::default(), [9; 32]).with_clear(true);
    let t = tree();
    let now = Cell::new(T0);
    let mut r = rig(&server, &t, &now, direct());
    let mut c = Client::new(creds(None), rights::READ, "/");
    // glenda, with a ticket, reads her own file; `none` cannot.
    let Ok(Owned::Read { data, .. }) = read(&mut c, &mut r, "/mine", None) else {
        panic!()
    };
    assert_eq!(data, b"yes");
    assert_eq!(
        clear_read(&mut r, "/mine", None),
        remote("permission denied")
    );
    assert!(clear_read(&mut r, "/hello", None).is_ok());
    let who: Vec<&str> = r
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Answered { uname, .. } => Some(uname.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(who, ["glenda", "none", "none"]);
    // A narrowed ticket stays narrow next to the clear mode.
    let mut narrow = Client::new(creds(None), rights::READ, "/lib");
    assert_eq!(
        read(&mut narrow, &mut r, "/hello", None),
        remote("permission denied")
    );
    // A stranger still gets no ticket: `-A` does not weaken the check.
    let stranger = Creds {
        user: String::from("glenda"),
        seed: [5; 32],
        cert: None,
    };
    let mut c = Client::new(stranger, rights::READ, "/");
    assert_eq!(
        read(&mut c, &mut r, "/hello", None),
        remote("authentication failed")
    );
}
