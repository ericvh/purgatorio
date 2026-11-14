//! The body of the `lethe` fuzz target, for the Lethe codec, tickets, the
//! server engine and the client's reply handling (`purg_lethe`).
//! `cargo test -p purg-lethe --test fuzz_smoke` includes this file too, so
//! the checks run without libFuzzer.
//!
//! It panics only when the code breaks one of its promises: what decodes
//! encodes back to the same bytes, and nothing panics on any input.

use purg_lethe::client::{self, Creds, Session};
use purg_lethe::msg::{Frame, Grant, Msg, Op, Reply, Request};
use purg_lethe::server::{Config, Env, Server, Tree, Trust};
use purg_lethe::ticket::{self, TicketKey};
use purg_styx::{DMDIR, Dir, QTDIR, QTFILE, Qid};

/// The ticket key and user seed the target's server and client use.
pub const KEY: [u8; 32] = [7; 32];
pub const SEED: [u8; 32] = [1; 32];
pub const NOW: u64 = 1_700_000_000;

/// A two-file tree.
pub struct Small;

impl Tree for Small {
    fn stat(&self, path: &str) -> Result<Dir, String> {
        let dir = path == "/";
        if !dir && path != "/f" {
            return Err(String::from("file does not exist"));
        }
        Ok(Dir {
            qid: Qid {
                path: path.len() as u64,
                vers: 1,
                qtype: if dir { QTDIR } else { QTFILE },
            },
            mode: 0o755 | if dir { DMDIR } else { 0 },
            length: if dir { 0 } else { 5 },
            name: String::from(if dir { "/" } else { "f" }),
            uid: String::from("u"),
            ..Dir::default()
        })
    }

    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(Qid, usize), String> {
        let d = self.stat(path)?;
        let data = b"hello";
        let off = (offset.min(5)) as usize;
        let n = buf.len().min(5 - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok((d.qid, n))
    }

    fn list(&self, path: &str, after: &[u8], limit: usize) -> Result<(Qid, Vec<Dir>), String> {
        let d = self.stat(path)?;
        let mut all = vec![self.stat("/f")?];
        all.retain(|e| e.name.as_bytes() > after);
        all.truncate(limit);
        Ok((d.qid, all))
    }
}

fn random(buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = i as u8 ^ 0x5a;
    }
}

fn server() -> Server {
    Server::new(Config::default(), KEY)
}

/// One call to the engine, which answers sealed requests only.
pub fn handle(frame: &[u8]) -> Vec<u8> {
    handle_with(&server(), frame)
}

/// One call to an engine that answers clear requests too (`lethed -A`).
pub fn handle_clear(frame: &[u8]) -> Vec<u8> {
    handle_with(&server().with_clear(true), frame)
}

fn handle_with(server: &Server, frame: &[u8]) -> Vec<u8> {
    let trust = Trust {
        keys: vec![purg_auth::sig::Id::Ed25519(purg_auth::public_key(&SEED))],
        signers: vec![],
        revoked: Some(vec![]),
    };
    let mut t = move |_: &str| trust.clone();
    let mut env = Env {
        now: NOW,
        random: &mut random,
        trust: &mut t,
    };
    server.handle(&Small, &mut env, frame).0
}

/// A session with the target's server, made through the real exchange.
pub fn session() -> Session {
    let creds = Creds {
        user: String::from("u"),
        seed: SEED,
        cert: None,
    };
    let (frame, pending) = client::auth(&creds, 1, purg_lethe::rights::READ, "/", [3; 32]);
    pending
        .finish(&handle(&frame), NOW)
        .expect("the target's own exchange works")
}

pub fn lethe(data: &[u8]) {
    // Frames: what decodes encodes back to itself.
    if let Ok(m) = Msg::decode(data) {
        assert_eq!(m.encode(), data, "frame round trip");
    }
    // Plaintexts.
    if let Ok(r) = Request::decode(data) {
        assert_eq!(r.encode(), data, "request round trip");
    }
    if let Ok(o) = Op::decode(data) {
        assert_eq!(o.encode(), data, "op round trip");
    }
    if let Ok(r) = Reply::decode(data) {
        assert_eq!(r.encode(), data, "reply round trip");
        let _ = r.owned();
    }
    if let Ok(g) = Grant::decode(data) {
        assert_eq!(g.encode(), data, "grant round trip");
    }
    let k = TicketKey::new(KEY);
    let _ = ticket::open(std::slice::from_ref(&k), data);
    // The server answers anything with a frame that decodes.
    let reply = handle(data);
    Msg::decode(&reply).expect("the server's reply is a frame");
    // A sealed-only server never answers a clear request.
    if let Ok(Msg {
        frame: Frame::Tclear { .. },
        ..
    }) = Msg::decode(data)
    {
        let m = Msg::decode(&reply).expect("checked above");
        assert!(
            matches!(m.frame, Frame::Rerror { ename } if ename == purg_lethe::error::AUTH_REQUIRED),
            "a clear request answered without -A"
        );
    }
    // A server that takes clear requests answers anything with a frame
    // too, and a clear reply holds a reply that decodes. The input is
    // tried as a frame and as a clear request's body.
    let body = &data[..data.len().min(purg_lethe::MAX_FRAME - purg_lethe::HEADER)];
    let as_body = Msg {
        id: 2,
        frame: Frame::Tclear { request: body },
    }
    .encode();
    for frame in [data, &as_body] {
        let reply = handle_clear(frame);
        let m = Msg::decode(&reply).expect("the clear server's reply is a frame");
        if let Frame::Rclear { reply } = m.frame {
            Reply::decode(reply).expect("a clear reply decodes");
        }
    }
    // The client takes any reply without panicking.
    let s = session();
    let (_, pending) = s.request(9, NOW, &Op::Stat { path: "/" }, [4; 24]);
    let _ = s.reply(&pending, data);
    let (_, pending) = client::clear(9, &Op::Stat { path: "/" });
    let _ = pending.reply(data);
    let creds = Creds {
        user: String::from("u"),
        seed: SEED,
        cert: None,
    };
    let (_, pending) = client::auth(&creds, 1, purg_lethe::rights::READ, "/", [3; 32]);
    let _ = pending.finish(data, NOW);
}
