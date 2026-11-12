//! The key schedule and the sealing of Lethe frames (`docs/lethe.md`,
//! Section 6). Everything sealed uses XChaCha20-Poly1305 ([`crate::aead`])
//! with a random 24-byte nonce, so neither side ever needs a counter it
//! would have to keep across requests.

use alloc::vec::Vec;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::aead;
use crate::error::MAC;
use crate::msg;
use crate::{EPH, HEADER, NONCE, TAG, kind};

const MASTER: &[u8] = b"lethe1 master";
const C2S: &[u8] = b"lethe1 c2s";
const S2C: &[u8] = b"lethe1 s2c";

/// The master secret of one ticket: HKDF-SHA256 of the X25519 secret,
/// salted with the SHA-256 of the transcript (the `Tauth` signed message
/// followed by the server's ephemeral key).
pub fn master(shared: &[u8; 32], transcript: &[u8]) -> [u8; 32] {
    let salt = Sha256::digest(transcript);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut m = [0u8; 32];
    // 32 bytes is always a valid HKDF-SHA256 output length.
    let _ = hk.expand(MASTER, &mut m);
    m
}

/// The transcript of an exchange.
pub fn transcript(signed: &[u8], server_eph: &[u8; EPH]) -> Vec<u8> {
    let mut t = Vec::with_capacity(signed.len() + EPH);
    t.extend_from_slice(signed);
    t.extend_from_slice(server_eph);
    t
}

/// The two directions' keys of a ticket.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Keys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
}

impl Keys {
    pub fn from_master(master: &[u8; 32]) -> Keys {
        let mut k = Keys {
            c2s: [0; 32],
            s2c: [0; 32],
        };
        // The master is already uniformly random: it is the PRK.
        if let Ok(hk) = Hkdf::<Sha256>::from_prk(master) {
            let _ = hk.expand(C2S, &mut k.c2s);
            let _ = hk.expand(S2C, &mut k.s2c);
        }
        k
    }
}

/// `plain` sealed under `key`: ciphertext and tag.
pub fn seal(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(plain.len() + TAG);
    v.extend_from_slice(plain);
    let tag = aead::seal(key, nonce, aad, &mut v);
    v.extend_from_slice(&tag);
    v
}

/// Opens what [`seal`] made. Fails with `bad record mac`.
pub fn open(
    key: &[u8; 32],
    nonce: &[u8; NONCE],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let at = sealed.len().checked_sub(TAG).ok_or(MAC)?;
    let (body, tag) = sealed.split_at(at);
    let tag: &[u8; TAG] = tag.try_into().map_err(|_| MAC)?;
    let mut v = body.to_vec();
    aead::open(key, nonce, aad, &mut v, tag)?;
    Ok(v)
}

fn concat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    v
}

/// A frame of type `ty` whose body is `prefix` and then `plain` sealed
/// under `key` with `aad`, built in one buffer: the plaintext is
/// encrypted where it lies and the tag appended.
fn sealed_frame(
    ty: u8,
    id: u32,
    prefix: &[&[u8]],
    key: &[u8; 32],
    nonce: &[u8; NONCE],
    aad_tail: &[u8],
    plain: &[u8],
) -> Vec<u8> {
    let pre: usize = prefix.iter().map(|p| p.len()).sum();
    let size = HEADER + pre + plain.len() + TAG;
    let h = msg::header(ty, id, size);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&h);
    for p in prefix {
        out.extend_from_slice(p);
    }
    let at = out.len();
    out.extend_from_slice(plain);
    let tag = aead::seal(key, nonce, &concat(&h, aad_tail), &mut out[at..]);
    out.extend_from_slice(&tag);
    out
}

/// A `Treq` frame: `request` (an encoded [`msg::Request`]) sealed under
/// `c2s`, with the header and the ticket as associated data.
pub fn treq(keys: &Keys, id: u32, ticket: &[u8], nonce: &[u8; NONCE], request: &[u8]) -> Vec<u8> {
    let len = (ticket.len() as u16).to_le_bytes();
    sealed_frame(
        kind::TREQ,
        id,
        &[&len, ticket, nonce],
        &keys.c2s,
        nonce,
        ticket,
        request,
    )
}

/// Opens a `Treq`'s sealed request. `frame` is the whole frame.
pub fn open_treq(
    keys: &Keys,
    frame: &[u8],
    ticket: &[u8],
    nonce: &[u8; NONCE],
    sealed: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let h = frame.get(..HEADER).ok_or(MAC)?;
    open(&keys.c2s, nonce, &concat(h, ticket), sealed)
}

/// An `Rreq` frame answering the request sealed with `req_nonce`.
pub fn rreq(
    keys: &Keys,
    id: u32,
    req_nonce: &[u8; NONCE],
    nonce: &[u8; NONCE],
    reply: &[u8],
) -> Vec<u8> {
    sealed_frame(kind::RREQ, id, &[nonce], &keys.s2c, nonce, req_nonce, reply)
}

/// Opens an `Rreq` answering the request sealed with `req_nonce`.
pub fn open_rreq(
    keys: &Keys,
    frame: &[u8],
    req_nonce: &[u8; NONCE],
    nonce: &[u8; NONCE],
    sealed: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let h = frame.get(..HEADER).ok_or(MAC)?;
    open(&keys.s2c, nonce, &concat(h, req_nonce), sealed)
}

/// An `Rauth` frame: `grant` sealed under `s2c`, with the header and the
/// server's ephemeral key as associated data.
pub fn rauth(keys: &Keys, id: u32, eph: &[u8; EPH], nonce: &[u8; NONCE], grant: &[u8]) -> Vec<u8> {
    sealed_frame(kind::RAUTH, id, &[eph, nonce], &keys.s2c, nonce, eph, grant)
}

/// Opens an `Rauth`'s grant.
pub fn open_rauth(
    keys: &Keys,
    frame: &[u8],
    eph: &[u8; EPH],
    nonce: &[u8; NONCE],
    sealed: &[u8],
) -> Result<Vec<u8>, &'static str> {
    let h = frame.get(..HEADER).ok_or(MAC)?;
    open(&keys.s2c, nonce, &concat(h, eph), sealed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{Frame, Msg};

    /// draft-irtf-cfrg-xchacha-03, appendix A.3.1: the AEAD vector, so the
    /// cipher is XChaCha20-Poly1305 as specified and not something close.
    #[test]
    fn xchacha_vector() {
        let hex = |s: &str| purg_auth::hex::decode(s.as_bytes()).unwrap();
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 24] = hex("404142434445464748494a4b4c4d4e4f5051525354555657")
            .try_into()
            .unwrap();
        let sealed = seal(&key, &nonce, &aad, plain);
        let n = sealed.len();
        assert_eq!(
            purg_auth::hex::encode(&sealed[n - TAG..]),
            "c0875924c1c7987947deafd8780acf49"
        );
        assert_eq!(
            purg_auth::hex::encode(&sealed[..16]),
            "bd6d179d3e83d43b9576579493c0e939"
        );
        assert_eq!(open(&key, &nonce, &aad, &sealed).unwrap(), plain);
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert_eq!(open(&key, &nonce, &aad, &bad), Err(MAC));
        assert_eq!(open(&key, &nonce, b"", &sealed), Err(MAC));
    }

    #[test]
    fn schedule() {
        let m = master(&[1; 32], b"t");
        assert_eq!(m, master(&[1; 32], b"t"));
        assert_ne!(m, master(&[1; 32], b"u"));
        assert_ne!(m, master(&[2; 32], b"t"));
        let k = Keys::from_master(&m);
        assert_ne!(k.c2s, k.s2c);
        assert_ne!(k, Keys::from_master(&[0; 32]));
    }

    #[test]
    fn frames_bind_header_and_request() {
        let k = Keys::from_master(&[3; 32]);
        let f = treq(&k, 7, b"ticket", &[1; NONCE], b"request");
        let m = Msg::decode(&f).unwrap();
        let Frame::Treq {
            ticket,
            nonce,
            sealed,
        } = m.frame
        else {
            panic!()
        };
        assert_eq!(
            open_treq(&k, &f, ticket, &nonce, sealed).unwrap(),
            b"request"
        );
        // Another id in the header, or another ticket, does not open.
        let mut g = f.clone();
        g[6] = 8;
        assert_eq!(open_treq(&k, &g, ticket, &nonce, sealed), Err(MAC));
        assert_eq!(open_treq(&k, &f, b"other!", &nonce, sealed), Err(MAC));
        // The server's key does not open the client's direction.
        let swapped = Keys {
            c2s: k.s2c,
            s2c: k.c2s,
        };
        assert_eq!(open_treq(&swapped, &f, ticket, &nonce, sealed), Err(MAC));

        let r = rreq(&k, 7, &[1; NONCE], &[2; NONCE], b"reply");
        let Frame::Rreq { nonce, sealed } = Msg::decode(&r).unwrap().frame else {
            panic!()
        };
        assert_eq!(
            open_rreq(&k, &r, &[1; NONCE], &nonce, sealed).unwrap(),
            b"reply"
        );
        // A reply moved onto another request does not open.
        assert_eq!(open_rreq(&k, &r, &[9; NONCE], &nonce, sealed), Err(MAC));
    }
}
