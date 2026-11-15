//! Plan 9's `p9any` / `p9sk1` authentication on a connection, both ends,
//! and the auth server's ticket service (`purg_auth::p9sk1` has the
//! formats and the checks; this does the reading and writing).
//!
//! Keys come from a key file of factotum key lines
//! (`key proto=p9sk1 dom=<dom> user=<user> !password=<password>`).

use std::io::{Read, Write};

use purg_auth::p9sk1::{
    self, AERRLEN, AUTH_ERR, AUTH_OK, AUTH_TREQ, AUTHENTLEN, CHALLEN, DESKEYLEN, KeyEntry,
    TICKETLEN, TICKREQLEN, TicketReq,
};

use crate::Error;
use crate::auth::Rng;

/// The longest p9any offer or choice read before its NUL.
const MAXMSG: usize = 4096;

/// The p9sk1 keys in the key file `path`.
pub fn keys(path: &str) -> Result<Vec<KeyEntry>, Error> {
    let text =
        std::fs::read_to_string(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    let keys = p9sk1::parse_keys(&text);
    if keys.is_empty() {
        return Err(Error(format!("{path}: no p9sk1 key")));
    }
    Ok(keys)
}

fn err(e: &str) -> Error {
    Error(format!("p9sk1: {e}"))
}

/// Reads up to and including a NUL, returning what came before it.
fn read_to_nul(conn: &mut impl Read) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if conn.read(&mut b)? == 0 {
            return Err(Error::from("p9any: connection closed"));
        }
        if b[0] == 0 {
            return Ok(out);
        }
        if out.len() == MAXMSG {
            return Err(Error::from("p9any: message too long"));
        }
        out.push(b[0]);
    }
}

fn read_exact(conn: &mut impl Read, buf: &mut [u8]) -> Result<(), Error> {
    conn.read_exact(buf)
        .map_err(|_| Error::from("p9any: connection closed"))
}

/// The server's side on `conn`, with its key `key`: offers p9sk1 in its
/// domain and checks the client. Returns the user the client
/// authenticated as (the ticket's `suid`).
pub fn server(conn: &mut (impl Read + Write), key: &KeyEntry) -> Result<String, Error> {
    conn.write_all(&p9sk1::offer(&key.dom))?;
    let choice = read_to_nul(conn)?;
    p9sk1::chosen(&choice, &key.dom).map_err(|e| Error(format!("p9any: {e}")))?;
    conn.write_all(b"OK\0")?;
    let mut cchal = [0u8; CHALLEN];
    read_exact(conn, &mut cchal)?;
    let mut chal = [0u8; CHALLEN];
    Rng::fill(&mut chal);
    let req = TicketReq {
        kind: AUTH_TREQ,
        authid: key.user.clone(),
        authdom: key.dom.clone(),
        chal,
        ..TicketReq::default()
    };
    conn.write_all(&req.encode())?;
    let mut msg = [0u8; TICKETLEN + AUTHENTLEN];
    read_exact(conn, &mut msg)?;
    let (t, auth) = p9sk1::server_check(&req, &msg, &key.key, &cchal).map_err(err)?;
    conn.write_all(&auth)?;
    Ok(t.suid)
}

/// The client's side on `conn`, with the keys `keys`: picks the offered
/// domain it has a key for, and gets tickets from the auth server at
/// `authsrv` (a dial string), or, with none, makes them itself if its key
/// is the server's. Returns the user it is on the server.
pub fn client(
    conn: &mut (impl Read + Write),
    keys: &[KeyEntry],
    authsrv: Option<&str>,
) -> Result<String, Error> {
    let offer = read_to_nul(conn)?;
    let v2 = offer.starts_with(b"v.2 ");
    let dom = keys
        .iter()
        .find_map(|k| p9sk1::choose(&offer, Some(&k.dom)).ok())
        .ok_or_else(|| {
            Error(format!(
                "p9any: no key for the domains offered: {}",
                String::from_utf8_lossy(&offer)
            ))
        })?;
    let (dom, reply) = dom;
    conn.write_all(&reply)?;
    if v2 && read_to_nul(conn)? != b"OK" {
        return Err(Error::from("p9any: server did not say OK"));
    }
    let key = keys
        .iter()
        .find(|k| k.dom == dom)
        .ok_or_else(|| Error::from("p9any: no key"))?;
    let mut cchal = [0u8; CHALLEN];
    Rng::fill(&mut cchal);
    conn.write_all(&cchal)?;
    let mut rb = [0u8; TICKREQLEN];
    read_exact(conn, &mut rb)?;
    let mut req = TicketReq::decode(&rb);
    if req.kind != AUTH_TREQ {
        return Err(err(p9sk1::EBADMSG));
    }
    req.hostid = key.user.clone();
    req.uid = key.user.clone();
    let tickets = match authsrv {
        Some(addr) => ask_authsrv(addr, &req)?,
        None if req.authid == key.user => {
            let mut nonce = [0u8; DESKEYLEN];
            Rng::fill(&mut nonce);
            p9sk1::own_tickets(&req, &key.key, nonce)
        }
        None => {
            return Err(Error(format!(
                "p9sk1: no auth server for domain {dom}, and the server is {}",
                req.authid
            )));
        }
    };
    let (t, msg) = p9sk1::client_reply(&req, &tickets, &key.key).map_err(err)?;
    conn.write_all(&msg)?;
    let mut auth = [0u8; AUTHENTLEN];
    read_exact(conn, &mut auth)?;
    p9sk1::client_check(&auth, &t, &cchal).map_err(err)?;
    Ok(t.suid)
}

/// Asks the auth server at `addr` for tickets for `req`.
fn ask_authsrv(addr: &str, req: &TicketReq) -> Result<[u8; 2 * TICKETLEN], Error> {
    // A bare host is its ticket port.
    let addr = if addr.contains('!') {
        crate::dial::netmkaddr(addr)
    } else {
        format!("tcp!{addr}!{}", p9sk1::TICKET_PORT)
    };
    let c = crate::dial::dial(&addr).map_err(|e| Error(format!("auth server {addr}: {e}")))?;
    let mut f = c
        .data
        .ok_or_else(|| Error::from("auth server: no data file"))
        .map(std::fs::File::from)?;
    f.write_all(&req.encode())?;
    let mut kind = [0u8; 1];
    read_exact(&mut f, &mut kind)?;
    match kind[0] {
        AUTH_OK => {
            let mut t = [0u8; 2 * TICKETLEN];
            read_exact(&mut f, &mut t)?;
            Ok(t)
        }
        AUTH_ERR => {
            let mut e = [0u8; AERRLEN];
            read_exact(&mut f, &mut e)?;
            let end = e.iter().position(|&b| b == 0).unwrap_or(e.len());
            Err(Error(format!(
                "auth server: remote: {}",
                String::from_utf8_lossy(&e[..end])
            )))
        }
        _ => Err(Error::from("auth server: bad reply")),
    }
}

/// The auth server's ticket service on `conn`, until it closes: each
/// ticket request is answered with tickets sealed with the keys of its
/// host (the client) and its auth id (the server), from `keys`, or with
/// an error. A host gets tickets only for itself.
pub fn ticket_service(conn: &mut (impl Read + Write), keys: &[KeyEntry]) -> Result<(), Error> {
    loop {
        let mut rb = [0u8; TICKREQLEN];
        if conn.read_exact(&mut rb).is_err() {
            return Ok(());
        }
        let req = TicketReq::decode(&rb);
        let find = |user: &str| {
            keys.iter()
                .find(|k| k.user == user && k.dom == req.authdom)
                .map(|k| k.key)
        };
        let reply = match (find(&req.hostid), find(&req.authid)) {
            _ if req.kind != AUTH_TREQ => p9sk1::auth_error("bad request type"),
            _ if req.uid != req.hostid => p9sk1::auth_error("host may not speak for user"),
            (None, _) => p9sk1::auth_error(&format!("no key for {}", req.hostid)),
            (_, None) => p9sk1::auth_error(&format!("no key for {}", req.authid)),
            (Some(kc), Some(ks)) => {
                let mut nonce = [0u8; DESKEYLEN];
                Rng::fill(&mut nonce);
                p9sk1::tickets(&req, &kc, &ks, nonce)
            }
        };
        conn.write_all(&reply)?;
    }
}
