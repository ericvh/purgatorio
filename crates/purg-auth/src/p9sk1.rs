//! Plan 9's `p9any` negotiation and its `p9sk1` protocol, for
//! authenticated interop with Plan 9 (`9export`, `import -9`), and the
//! auth server's ticket request (Plan 9's authsrv(6)).
//!
//! p9sk1 is a shared-key (DES) protocol: each user's key is known to the
//! auth server of their domain, which issues a pair of tickets that let a
//! client and a server agree on a nonce key and prove their identities to
//! each other. It is old and weak (56-bit DES), and is here only to talk
//! to Plan 9; Purgatorio's own mounts use Ed25519 (`docs/auth.md`).
//!
//! The conversation, after the server's offer `v.2 p9sk1@dom NUL` and the
//! client's choice `p9sk1 dom NUL`, answered `OK NUL`:
//!
//! ```text
//! C->S  CHc[8]                                the client's challenge
//! S->C  ticketreq[141]                        type AuthTreq, the server's
//!                                             id and domain, CHs[8]
//! C->AS ticketreq[141] (hostid, uid filled)   the client asks the auth server
//! AS->C AuthOK, Kc{ticket}[72], Ks{ticket}[72]
//! C->S  Ks{ticket}[72], Kn{authenticator}[13] (AuthAc, CHs)
//! S->C  Kn{authenticator}[13]                 (AuthAs, CHc)
//! ```
//!
//! A ticket is `num, chal[8], cuid[28], suid[28], Kn[7]`; an
//! authenticator `num, chal[8], id[4]` (little-endian). Both are sealed
//! with Plan 9's `encrypt`: DES over 8-byte windows stepping 7 bytes, so
//! a message of any length from 8 bytes is covered.

use alloc::string::String;
use alloc::vec::Vec;

use des::Des;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

/// A name field (user, auth id, host id).
pub const ANAMELEN: usize = 28;
/// An authentication domain field.
pub const DOMLEN: usize = 48;
/// A DES key in Plan 9's 7-byte form.
pub const DESKEYLEN: usize = 7;
/// A p9sk1 challenge.
pub const CHALLEN: usize = 8;
/// An auth server error message.
pub const AERRLEN: usize = 64;
pub const TICKREQLEN: usize = 3 * ANAMELEN + CHALLEN + DOMLEN + 1;
pub const TICKETLEN: usize = CHALLEN + 2 * ANAMELEN + DESKEYLEN + 1;
pub const AUTHENTLEN: usize = CHALLEN + 4 + 1;

/// Message types and replay numbers, as authsrv(6) numbers them.
pub const AUTH_TREQ: u8 = 1;
pub const AUTH_OK: u8 = 4;
pub const AUTH_ERR: u8 = 5;
pub const AUTH_TS: u8 = 64;
pub const AUTH_TC: u8 = 65;
pub const AUTH_AS: u8 = 66;
pub const AUTH_AC: u8 = 67;

/// The auth server's ticket service port (`ticket` in Plan 9's ndb).
pub const TICKET_PORT: u16 = 567;

/// A user's DES key, from a password ([`passtokey`]) or given in hex.
pub type Key = [u8; DESKEYLEN];

/// The errors Plan 9 gives, as it words them.
pub const EKEYMISMATCH: &str = "key mismatch with auth server";
pub const ESERVERLIES: &str = "server lies through his teeth";
pub const ECLIENTLIES: &str = "client lies through his teeth";
pub const EBADMSG: &str = "bad protocol message";
pub const ENOPROTO: &str = "no p9sk1 in the offer";

/// A 7-byte key spread over 8 bytes, 7 bits a byte, as DES takes it
/// (the low bit of each byte is parity, which DES ignores).
pub fn des56to64(k: &Key) -> [u8; 8] {
    let hi = u32::from_be_bytes([k[0], k[1], k[2], k[3]]);
    let lo = u32::from_be_bytes([k[4], k[5], k[6], 0]);
    let b = |v: u32| ((v & 0x7f) as u8) << 1;
    [
        b(hi >> 25),
        b(hi >> 18),
        b(hi >> 11),
        b(hi >> 4),
        b((hi << 3) | (lo >> 29)),
        b(lo >> 22),
        b(lo >> 15),
        b(lo >> 8),
    ]
}

fn cipher(k: &Key) -> Des {
    // An 8-byte key is the one length DES takes.
    Des::new_from_slice(&des56to64(k)).unwrap_or_else(|_| unreachable!())
}

/// Plan 9's `encrypt`: seals `buf` in place, which must be at least 8
/// bytes: DES on each 8-byte window, stepping 7 bytes, and on the last 8
/// bytes when the length leaves a remainder. False if `buf` is shorter.
pub fn encrypt(key: &Key, buf: &mut [u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    let c = cipher(key);
    let n = buf.len() - 1;
    let (blocks, r) = (n / 7, n % 7);
    for i in 0..blocks {
        c.encrypt_block(GenericArray::from_mut_slice(&mut buf[i * 7..i * 7 + 8]));
    }
    if r != 0 {
        let at = blocks * 7 - 7 + r;
        c.encrypt_block(GenericArray::from_mut_slice(&mut buf[at..at + 8]));
    }
    true
}

/// Plan 9's `decrypt`, the inverse of [`encrypt`].
pub fn decrypt(key: &Key, buf: &mut [u8]) -> bool {
    if buf.len() < 8 {
        return false;
    }
    let c = cipher(key);
    let n = buf.len() - 1;
    let (blocks, r) = (n / 7, n % 7);
    if r != 0 {
        let at = blocks * 7 - 7 + r;
        c.decrypt_block(GenericArray::from_mut_slice(&mut buf[at..at + 8]));
    }
    for i in (0..blocks).rev() {
        c.decrypt_block(GenericArray::from_mut_slice(&mut buf[i * 7..i * 7 + 8]));
    }
    true
}

/// Plan 9's `passtokey`: a password as a DES key. The password is cut at
/// 27 bytes and padded with spaces to 8; each 8-byte stretch folds into
/// the key, and the key encrypts the next stretch before it is folded in.
pub fn passtokey(password: &str) -> Key {
    let p = password.as_bytes();
    let mut n = p.len().min(ANAMELEN - 1);
    let mut buf = [b' '; ANAMELEN + 8];
    buf[..n].copy_from_slice(&p[..n]);
    buf[n] = 0;
    let mut key = [0u8; DESKEYLEN];
    let mut t = 0;
    loop {
        for (i, k) in key.iter_mut().enumerate() {
            *k = (buf[t + i] >> i).wrapping_add(buf[t + i + 1] << (7 - i));
        }
        if n <= 8 {
            return key;
        }
        n -= 8;
        t += 8;
        if n < 8 {
            t -= 8 - n;
            n = 8;
        }
        let k = key;
        encrypt(&k, &mut buf[t..t + 8]);
    }
}

/// A key given as 14 hex digits (factotum's `!hex`).
pub fn hexkey(s: &str) -> Option<Key> {
    crate::hex::array::<DESKEYLEN>(s.as_bytes())
}

/// A p9sk1 key from a key file: a user, their domain and their key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub user: String,
    pub dom: String,
    pub key: Key,
}

/// The p9sk1 keys in `text`, factotum's key lines (Plan 9's factotum(4)):
/// `key proto=p9sk1 dom=<dom> user=<user> !password=<password>`, or
/// `!hex=<14 hex digits>` in place of the password. Other lines, and
/// lines without a user, a domain or a key, are passed over.
pub fn parse_keys(text: &str) -> Vec<KeyEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("key ").unwrap_or(line);
        let Some(attrs) = crate::attr::parse(line) else {
            continue;
        };
        let get = |n| crate::attr::get(&attrs, n);
        if get("proto") != Some("p9sk1") {
            continue;
        }
        let (Some(user), Some(dom)) = (get("user"), get("dom")) else {
            continue;
        };
        let key = match (get("!password"), get("!hex")) {
            (Some(p), _) => passtokey(p),
            (None, Some(h)) => match hexkey(h) {
                Some(k) => k,
                None => continue,
            },
            _ => continue,
        };
        if fits(user, ANAMELEN) && fits(dom, DOMLEN) {
            out.push(KeyEntry {
                user: String::from(user),
                dom: String::from(dom),
                key,
            });
        }
    }
    out
}

fn put_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let b = s.as_bytes();
    let n = b.len().min(len - 1);
    out.extend_from_slice(&b[..n]);
    out.resize(out.len() + len - n, 0);
}

fn get_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Whether `s` fits a field of `len` bytes with its NUL.
pub fn fits(s: &str, len: usize) -> bool {
    s.len() < len && !s.as_bytes().contains(&0)
}

/// A ticket request: what the server sends the client, and the client
/// the auth server with `hostid` and `uid` filled in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TicketReq {
    pub kind: u8,
    /// The server's id: whose key seals the server's ticket.
    pub authid: String,
    pub authdom: String,
    /// The server's challenge.
    pub chal: [u8; CHALLEN],
    /// The client's id: whose key seals the client's ticket.
    pub hostid: String,
    /// The user to be on the server.
    pub uid: String,
}

impl TicketReq {
    pub fn encode(&self) -> [u8; TICKREQLEN] {
        let mut v = Vec::with_capacity(TICKREQLEN);
        v.push(self.kind);
        put_str(&mut v, &self.authid, ANAMELEN);
        put_str(&mut v, &self.authdom, DOMLEN);
        v.extend_from_slice(&self.chal);
        put_str(&mut v, &self.hostid, ANAMELEN);
        put_str(&mut v, &self.uid, ANAMELEN);
        let mut out = [0u8; TICKREQLEN];
        out.copy_from_slice(&v);
        out
    }

    pub fn decode(b: &[u8; TICKREQLEN]) -> TicketReq {
        let mut at = 1;
        let mut take = |n: usize| {
            let s = &b[at..at + n];
            at += n;
            s
        };
        let authid = get_str(take(ANAMELEN));
        let authdom = get_str(take(DOMLEN));
        let mut chal = [0u8; CHALLEN];
        chal.copy_from_slice(take(CHALLEN));
        let hostid = get_str(take(ANAMELEN));
        let uid = get_str(take(ANAMELEN));
        TicketReq {
            kind: b[0],
            authid,
            authdom,
            chal,
            hostid,
            uid,
        }
    }
}

/// A ticket, sealed with the key of the party it is for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ticket {
    pub num: u8,
    pub chal: [u8; CHALLEN],
    /// Who the client is.
    pub cuid: String,
    /// Who the client is to be on the server.
    pub suid: String,
    /// The nonce key the two share for the rest of the conversation.
    pub key: Key,
}

impl Ticket {
    pub fn encode(&self, seal: &Key) -> [u8; TICKETLEN] {
        let mut v = Vec::with_capacity(TICKETLEN);
        v.push(self.num);
        v.extend_from_slice(&self.chal);
        put_str(&mut v, &self.cuid, ANAMELEN);
        put_str(&mut v, &self.suid, ANAMELEN);
        v.extend_from_slice(&self.key);
        let mut out = [0u8; TICKETLEN];
        out.copy_from_slice(&v);
        encrypt(seal, &mut out);
        out
    }

    pub fn decode(b: &[u8; TICKETLEN], seal: &Key) -> Ticket {
        let mut b = *b;
        decrypt(seal, &mut b);
        let mut chal = [0u8; CHALLEN];
        chal.copy_from_slice(&b[1..1 + CHALLEN]);
        let mut key = [0u8; DESKEYLEN];
        key.copy_from_slice(&b[TICKETLEN - DESKEYLEN..]);
        Ticket {
            num: b[0],
            chal,
            cuid: get_str(&b[1 + CHALLEN..1 + CHALLEN + ANAMELEN]),
            suid: get_str(&b[1 + CHALLEN + ANAMELEN..1 + CHALLEN + 2 * ANAMELEN]),
            key,
        }
    }
}

/// An authenticator, sealed with the nonce key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Authenticator {
    pub num: u8,
    pub chal: [u8; CHALLEN],
    pub id: u32,
}

impl Authenticator {
    pub fn encode(&self, seal: &Key) -> [u8; AUTHENTLEN] {
        let mut out = [0u8; AUTHENTLEN];
        out[0] = self.num;
        out[1..1 + CHALLEN].copy_from_slice(&self.chal);
        out[1 + CHALLEN..].copy_from_slice(&self.id.to_le_bytes());
        encrypt(seal, &mut out);
        out
    }

    pub fn decode(b: &[u8; AUTHENTLEN], seal: &Key) -> Authenticator {
        let mut b = *b;
        decrypt(seal, &mut b);
        let mut chal = [0u8; CHALLEN];
        chal.copy_from_slice(&b[1..1 + CHALLEN]);
        Authenticator {
            num: b[0],
            chal,
            id: u32::from_le_bytes([b[9], b[10], b[11], b[12]]),
        }
    }
}

/// The server's p9any offer for `dom`, with its NUL.
pub fn offer(dom: &str) -> Vec<u8> {
    let mut v = alloc::format!("v.2 p9sk1@{dom}").into_bytes();
    v.push(0);
    v
}

/// The client's choice for an offer (without its NUL): the domain of
/// its `p9sk1` entry (`dom`'s if given and offered), and the reply to
/// send, with its NUL. Version 1 offers (no `v.2`) are taken too.
pub fn choose(offer: &[u8], dom: Option<&str>) -> Result<(String, Vec<u8>), &'static str> {
    let text = core::str::from_utf8(offer).map_err(|_| EBADMSG)?;
    let mut words = text.split_ascii_whitespace().peekable();
    if words.peek() == Some(&"v.2") {
        words.next();
    }
    let found = words
        .filter_map(|w| w.split_once('@'))
        .filter(|(p, _)| *p == "p9sk1")
        .map(|(_, d)| d)
        .find(|d| dom.is_none_or(|want| want == *d))
        .ok_or(ENOPROTO)?;
    let mut reply = alloc::format!("p9sk1 {found}").into_bytes();
    reply.push(0);
    Ok((String::from(found), reply))
}

/// The server's reading of the client's choice (without its NUL): the
/// domain, which must be `dom`.
pub fn chosen(msg: &[u8], dom: &str) -> Result<(), &'static str> {
    let text = core::str::from_utf8(msg).map_err(|_| EBADMSG)?;
    let mut w = text.split_ascii_whitespace();
    match (w.next(), w.next(), w.next()) {
        (Some("p9sk1"), Some(d), None) if d == dom => Ok(()),
        (Some("p9sk1"), Some(_), None) => Err("p9sk1: wrong domain"),
        (Some(_), Some(_), None) => Err("bad chosen protocol"),
        _ => Err(EBADMSG),
    }
}

/// The auth server's answer to a ticket request, given the keys of its
/// `hostid` (the client) and `authid` (the server) and a fresh nonce key:
/// `AuthOK`, the client's ticket and the server's, as Plan 9's authsrv
/// makes them (`cuid` is the host id, `suid` the user).
pub fn tickets(req: &TicketReq, client: &Key, server: &Key, nonce: Key) -> Vec<u8> {
    let mut t = Ticket {
        num: AUTH_TC,
        chal: req.chal,
        cuid: req.hostid.clone(),
        suid: req.uid.clone(),
        key: nonce,
    };
    let mut out = Vec::with_capacity(1 + 2 * TICKETLEN);
    out.push(AUTH_OK);
    out.extend_from_slice(&t.encode(client));
    t.num = AUTH_TS;
    out.extend_from_slice(&t.encode(server));
    out
}

/// The auth server's refusal: `AuthErr` and the reason in 64 bytes.
pub fn auth_error(msg: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + AERRLEN);
    out.push(AUTH_ERR);
    put_str(&mut out, msg, AERRLEN);
    out
}

/// Tickets the client makes itself, as Plan 9's factotum does when its
/// own key is the server's (`authid` equals `hostid`) and no auth server
/// answers: both are sealed with that key.
pub fn own_tickets(req: &TicketReq, key: &Key, nonce: Key) -> [u8; 2 * TICKETLEN] {
    let mut t = Ticket {
        num: AUTH_TC,
        chal: req.chal,
        cuid: req.uid.clone(),
        suid: req.uid.clone(),
        key: nonce,
    };
    let mut out = [0u8; 2 * TICKETLEN];
    out[..TICKETLEN].copy_from_slice(&t.encode(key));
    t.num = AUTH_TS;
    out[TICKETLEN..].copy_from_slice(&t.encode(key));
    out
}

/// The client, given the two tickets (its own first) for `req`: its
/// ticket, and what to send the server, the server's ticket and an
/// authenticator for the server's challenge.
pub fn client_reply(
    req: &TicketReq,
    tickets: &[u8; 2 * TICKETLEN],
    key: &Key,
) -> Result<(Ticket, [u8; TICKETLEN + AUTHENTLEN]), &'static str> {
    let mine: &[u8; TICKETLEN] = tickets[..TICKETLEN].try_into().map_err(|_| EBADMSG)?;
    let t = Ticket::decode(mine, key);
    if t.num != AUTH_TC || t.chal != req.chal {
        return Err(EKEYMISMATCH);
    }
    let mut out = [0u8; TICKETLEN + AUTHENTLEN];
    out[..TICKETLEN].copy_from_slice(&tickets[TICKETLEN..]);
    let a = Authenticator {
        num: AUTH_AC,
        chal: req.chal,
        id: 0,
    };
    out[TICKETLEN..].copy_from_slice(&a.encode(&t.key));
    Ok((t, out))
}

/// The client's check of the server's authenticator: it must answer the
/// client's challenge with the nonce key.
pub fn client_check(
    auth: &[u8; AUTHENTLEN],
    t: &Ticket,
    cchal: &[u8; CHALLEN],
) -> Result<(), &'static str> {
    let a = Authenticator::decode(auth, &t.key);
    if a.num != AUTH_AS || a.chal != *cchal || a.id != 0 {
        return Err(ESERVERLIES);
    }
    Ok(())
}

/// The server, given the client's ticket and authenticator for `req`:
/// the ticket, whose `suid` is the authenticated user, and the
/// authenticator to send back for the client's challenge.
pub fn server_check(
    req: &TicketReq,
    msg: &[u8; TICKETLEN + AUTHENTLEN],
    key: &Key,
    cchal: &[u8; CHALLEN],
) -> Result<(Ticket, [u8; AUTHENTLEN]), &'static str> {
    let tb: &[u8; TICKETLEN] = msg[..TICKETLEN].try_into().map_err(|_| EBADMSG)?;
    let t = Ticket::decode(tb, key);
    if t.num != AUTH_TS || t.chal != req.chal {
        return Err(EKEYMISMATCH);
    }
    let ab: &[u8; AUTHENTLEN] = msg[TICKETLEN..].try_into().map_err(|_| EBADMSG)?;
    let a = Authenticator::decode(ab, &t.key);
    if a.num != AUTH_AC || a.chal != req.chal || a.id != 0 {
        return Err(ECLIENTLIES);
    }
    let reply = Authenticator {
        num: AUTH_AS,
        chal: *cchal,
        id: 0,
    };
    let sealed = reply.encode(&t.key);
    Ok((t, sealed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(TICKREQLEN, 141);
        assert_eq!(TICKETLEN, 72);
        assert_eq!(AUTHENTLEN, 13);
    }

    /// DES itself, with the FIPS 81 example (key 0123456789abcdef, "Now is
    /// t"), through Plan 9's 7-byte key form: the 56 key bits of that
    /// key, packed 7 to a byte, give back the 8-byte key less parity.
    #[test]
    fn des_known_answer() {
        let k64 = [0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let mut bits = 0u64;
        for b in k64 {
            bits = (bits << 7) | u64::from(b >> 1);
        }
        let k56: Key = bits.to_be_bytes()[1..].try_into().unwrap();
        let spread = des56to64(&k56);
        for (a, b) in spread.iter().zip(k64) {
            assert_eq!(a >> 1, b >> 1);
        }
        let mut block = *b"Now is t";
        assert!(encrypt(&k56, &mut block));
        assert_eq!(block, [0x3f, 0xa4, 0x0e, 0x8a, 0x98, 0x4d, 0x48, 0x15]);
        assert!(decrypt(&k56, &mut block));
        assert_eq!(&block, b"Now is t");
    }

    #[test]
    fn encrypt_covers_every_length() {
        let key = passtokey("secret");
        for n in 8..80 {
            let plain: Vec<u8> = (0..n as u8).collect();
            let mut b = plain.clone();
            assert!(encrypt(&key, &mut b));
            assert_ne!(b, plain, "{n}");
            // Every byte is covered by some window.
            assert!(decrypt(&key, &mut b));
            assert_eq!(b, plain, "{n}");
        }
        assert!(!encrypt(&key, &mut [0u8; 7]));
    }

    #[test]
    fn passtokey_folds_and_differs() {
        assert_ne!(passtokey("a"), passtokey("b"));
        assert_ne!(passtokey("password"), passtokey("password1"));
        // Only the first 27 bytes count.
        let long = "x".repeat(40);
        assert_eq!(passtokey(&long), passtokey(&long[..27]));
        // A short password is padded with spaces: "ab" and "ab " differ
        // by the NUL after the password.
        assert_ne!(passtokey("ab"), passtokey("ab "));
    }

    #[test]
    fn formats_round_trip() {
        let req = TicketReq {
            kind: AUTH_TREQ,
            authid: "bootes".into(),
            authdom: "example.com".into(),
            chal: [1, 2, 3, 4, 5, 6, 7, 8],
            hostid: "glenda".into(),
            uid: "glenda".into(),
        };
        assert_eq!(TicketReq::decode(&req.encode()), req);
        let k = passtokey("k");
        let t = Ticket {
            num: AUTH_TS,
            chal: req.chal,
            cuid: "glenda".into(),
            suid: "glenda".into(),
            key: [9; 7],
        };
        assert_eq!(Ticket::decode(&t.encode(&k), &k), t);
        let a = Authenticator {
            num: AUTH_AC,
            chal: req.chal,
            id: 0x01020304,
        };
        assert_eq!(Authenticator::decode(&a.encode(&k), &k), a);
    }

    #[test]
    fn key_files() {
        let keys = parse_keys(
            "key proto=p9sk1 dom=d user=glenda !password=pw\n\
             key proto=ed25519 !seed=00\n\
             proto=p9sk1 dom=d user=bootes !hex=0102030405060a\n\
             key proto=p9sk1 user=nodom !password=x\n",
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, passtokey("pw"));
        assert_eq!(keys[1].user, "bootes");
        assert_eq!(keys[1].key, [1, 2, 3, 4, 5, 6, 0x0a]);
    }

    #[test]
    fn negotiation() {
        assert_eq!(offer("d"), b"v.2 p9sk1@d\0");
        let (dom, reply) = choose(b"v.2 p9sk2@x p9sk1@d", None).unwrap();
        assert_eq!((dom.as_str(), reply.as_slice()), ("d", &b"p9sk1 d\0"[..]));
        assert!(choose(b"v.2 p9sk1@d", Some("e")).is_err());
        assert!(choose(b"p9sk1@d", None).is_ok());
        assert!(choose(b"v.2 dp9ik@d", None).is_err());
        assert!(chosen(b"p9sk1 d", "d").is_ok());
        assert!(chosen(b"p9sk1 e", "d").is_err());
        assert!(chosen(b"p9sk2 d", "d").is_err());
    }

    /// A whole conversation, with an auth server that knows both keys,
    /// and with a wrong client key.
    #[test]
    fn a_conversation() {
        let (kc, ks) = (passtokey("client pw"), passtokey("server pw"));
        let cchal = [7u8; CHALLEN];
        let req = TicketReq {
            kind: AUTH_TREQ,
            authid: "server".into(),
            authdom: "d".into(),
            chal: [3u8; CHALLEN],
            hostid: "glenda".into(),
            uid: "glenda".into(),
        };
        let reply = tickets(&req, &kc, &ks, [5u8; 7]);
        assert_eq!(reply[0], AUTH_OK);
        let pair: [u8; 2 * TICKETLEN] = reply[1..].try_into().unwrap();
        let (t, msg) = client_reply(&req, &pair, &kc).unwrap();
        assert_eq!(t.key, [5u8; 7]);
        let (st, auth) = server_check(&req, &msg, &ks, &cchal).unwrap();
        assert_eq!((st.cuid.as_str(), st.suid.as_str()), ("glenda", "glenda"));
        client_check(&auth, &t, &cchal).unwrap();
        // A client with the wrong key cannot read its ticket.
        assert_eq!(
            client_reply(&req, &pair, &passtokey("wrong")).unwrap_err(),
            EKEYMISMATCH
        );
        // A server with the wrong key cannot read its ticket.
        assert_eq!(
            server_check(&req, &msg, &passtokey("wrong"), &cchal).unwrap_err(),
            EKEYMISMATCH
        );
        // An answer to another challenge is a lie.
        assert_eq!(
            client_check(&auth, &t, &[0u8; CHALLEN]).unwrap_err(),
            ESERVERLIES
        );
        // Tickets a client makes with the server's own key.
        let own = own_tickets(&req, &ks, [6u8; 7]);
        let (t, msg) = client_reply(&req, &own, &ks).unwrap();
        let (_, auth) = server_check(&req, &msg, &ks, &cchal).unwrap();
        client_check(&auth, &t, &cchal).unwrap();
    }
}
