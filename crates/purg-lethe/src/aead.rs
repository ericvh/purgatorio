//! XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha-03), the AEAD every sealed
//! Lethe message uses, from one of two backends that make the same bytes:
//!
//! - **portable** (the kernel and anything without the `ring` feature):
//!   the RFC 8439 construction over `chacha20`'s XChaCha20 keystream and
//!   `poly1305`. `chacha20` 0.10 uses its NEON backend on aarch64 targets
//!   with `neon` (the kernel's `aarch64-unknown-none` has it), chosen at
//!   compile time, so the code is the same on every build host.
//! - **ring** (the `ring` feature: the hosted emu and `lethed`): XChaCha20
//!   is ChaCha20 under a subkey. HChaCha20 of the key and the first 16
//!   bytes of the nonce gives the subkey; ring's ChaCha20-Poly1305 then
//!   seals under it with the nonce `0[4] || nonce[16..24]`. Only the
//!   HChaCha20 block is portable code; the bulk is ring's assembly.
//!
//! The tests check both against the RustCrypto `XChaCha20Poly1305`, in
//! both directions, and against the draft's vector.

use crate::{NONCE, TAG};

/// Seals `buf` in place and returns the tag.
pub fn seal(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], buf: &mut [u8]) -> [u8; TAG] {
    backend::seal(key, nonce, aad, buf)
}

/// Opens `buf` in place, checking `tag`: `bad record mac` when it does
/// not match. On failure `buf` is not unchecked plaintext.
pub fn open(
    key: &[u8; 32],
    nonce: &[u8; NONCE],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8; TAG],
) -> Result<(), &'static str> {
    backend::open(key, nonce, aad, buf, tag)
}

#[cfg(not(feature = "ring"))]
use portable as backend;
#[cfg(feature = "ring")]
use ring_backend as backend;

/// The RFC 8439 AEAD over the XChaCha20 keystream.
#[cfg_attr(feature = "ring", allow(dead_code))]
pub(crate) mod portable {
    use chacha20::XChaCha20;
    use chacha20::cipher::{KeyIvInit, StreamCipher};
    use poly1305::Poly1305;
    use poly1305::universal_hash::{KeyInit, UniversalHash};

    use crate::error::MAC;
    use crate::{NONCE, TAG};

    /// The cipher positioned at block 1, and the Poly1305 instance keyed
    /// with block 0, as RFC 8439 Section 2.8 does.
    fn start(key: &[u8; 32], nonce: &[u8; NONCE]) -> (XChaCha20, Poly1305) {
        let mut c = XChaCha20::new(key.into(), nonce.into());
        let mut block0 = [0u8; 64];
        c.apply_keystream(&mut block0);
        let mac = Poly1305::new(poly1305::Key::from_slice(&block0[..32]));
        (c, mac)
    }

    fn lengths(mac: &mut Poly1305, aad: usize, text: usize) {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&(aad as u64).to_le_bytes());
        b[8..].copy_from_slice(&(text as u64).to_le_bytes());
        mac.update(&[b.into()]);
    }

    pub fn seal(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], buf: &mut [u8]) -> [u8; TAG] {
        let (mut c, mut mac) = start(key, nonce);
        c.apply_keystream(buf);
        mac.update_padded(aad);
        mac.update_padded(buf);
        lengths(&mut mac, aad.len(), buf.len());
        mac.finalize().into()
    }

    pub fn open(
        key: &[u8; 32],
        nonce: &[u8; NONCE],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG],
    ) -> Result<(), &'static str> {
        let (mut c, mut mac) = start(key, nonce);
        mac.update_padded(aad);
        mac.update_padded(buf);
        lengths(&mut mac, aad.len(), buf.len());
        // Constant time, from `subtle`.
        mac.verify(tag.into()).map_err(|_| MAC)?;
        c.apply_keystream(buf);
        Ok(())
    }
}

/// ring's ChaCha20-Poly1305 under the HChaCha20 subkey.
#[cfg(feature = "ring")]
pub(crate) mod ring_backend {
    use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};

    use crate::error::MAC;
    use crate::{NONCE, TAG};

    /// XChaCha20's subkey and ChaCha20 nonce for `nonce`.
    pub(crate) fn subkey(key: &[u8; 32], nonce: &[u8; NONCE]) -> ([u8; 32], [u8; 12]) {
        let mut input = [0u8; 16];
        input.copy_from_slice(&nonce[..16]);
        let sub = chacha20::hchacha::<chacha20::R20>(key.into(), &input.into());
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&nonce[16..]);
        (sub.into(), n)
    }

    fn key(key: &[u8; 32], nonce: &[u8; NONCE]) -> (LessSafeKey, Nonce) {
        let (sub, n) = subkey(key, nonce);
        // A 32-byte key is always the right length.
        let k = UnboundKey::new(&CHACHA20_POLY1305, &sub).unwrap_or_else(|_| unreachable!());
        (LessSafeKey::new(k), Nonce::assume_unique_for_key(n))
    }

    pub fn seal(key_: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], buf: &mut [u8]) -> [u8; TAG] {
        let (k, n) = key(key_, nonce);
        // ring refuses only inputs beyond ChaCha20's 256 GiB.
        match k.seal_in_place_separate_tag(n, Aad::from(aad), buf) {
            Ok(t) => t.as_ref().try_into().unwrap_or([0; TAG]),
            Err(_) => [0; TAG],
        }
    }

    pub fn open(
        key_: &[u8; 32],
        nonce: &[u8; NONCE],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG],
    ) -> Result<(), &'static str> {
        let (k, n) = key(key_, nonce);
        let tag = ring::aead::Tag::from(*tag);
        k.open_in_place_separate_tag(n, Aad::from(aad), tag, buf, 0..)
            .map(drop)
            .map_err(|_| MAC)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    use super::*;
    use crate::error::MAC;

    type Seal = fn(&[u8; 32], &[u8; NONCE], &[u8], &mut [u8]) -> [u8; TAG];
    type Open =
        fn(&[u8; 32], &[u8; NONCE], &[u8], &mut [u8], &[u8; TAG]) -> Result<(), &'static str>;

    /// Every backend built here.
    fn backends() -> Vec<(&'static str, Seal, Open)> {
        let portable: (&'static str, Seal, Open) = ("portable", portable::seal, portable::open);
        #[cfg(feature = "ring")]
        return alloc::vec![portable, ("ring", ring_backend::seal, ring_backend::open)];
        #[cfg(not(feature = "ring"))]
        alloc::vec![portable]
    }

    /// Deterministic bytes.
    fn bytes(n: usize, seed: u8) -> Vec<u8> {
        let mut x = u32::from(seed).wrapping_mul(2_654_435_761) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    fn reference(key: &[u8; 32], nonce: &[u8; NONCE], aad: &[u8], msg: &[u8]) -> Vec<u8> {
        XChaCha20Poly1305::new(key.into())
            .encrypt(XNonce::from_slice(nonce), Payload { msg, aad })
            .unwrap()
    }

    /// Lengths around the 16-byte Poly1305 block and the 64-byte ChaCha20
    /// block (and the 4-block NEON stride), up to Lethe's largest reply.
    const LENS: &[usize] = &[
        0, 1, 15, 16, 17, 63, 64, 65, 127, 128, 255, 256, 257, 1000, 4096, 8192, 8203, 32768, 32799,
    ];

    #[test]
    fn agrees_with_rustcrypto_both_ways() {
        for (name, seal, open) in backends() {
            for (i, &n) in LENS.iter().enumerate() {
                let key: [u8; 32] = bytes(32, i as u8).try_into().unwrap();
                let nonce: [u8; NONCE] = bytes(NONCE, 100 + i as u8).try_into().unwrap();
                let aad = bytes(i * 7 % 40, 200 + i as u8);
                let msg = bytes(n, 50 + i as u8);
                let want = reference(&key, &nonce, &aad, &msg);
                // Ours seals what RustCrypto seals.
                let mut buf = msg.clone();
                let tag = seal(&key, &nonce, &aad, &mut buf);
                buf.extend_from_slice(&tag);
                assert_eq!(buf, want, "{name}: seal, {n} bytes");
                // Ours opens what RustCrypto sealed.
                let (ct, t) = want.split_at(n);
                let mut buf = ct.to_vec();
                open(&key, &nonce, &aad, &mut buf, t.try_into().unwrap()).unwrap();
                assert_eq!(buf, msg, "{name}: open, {n} bytes");
                // RustCrypto opens what ours sealed (the same bytes, so this
                // also checks the reference against itself).
                let back = XChaCha20Poly1305::new((&key).into())
                    .decrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: &want,
                            aad: &aad,
                        },
                    )
                    .unwrap();
                assert_eq!(back, msg);
                // A changed byte anywhere, in the text, the tag or the
                // associated data, does not open.
                for at in [0, n / 2, n.saturating_sub(1)] {
                    if n == 0 {
                        break;
                    }
                    let mut bad = ct.to_vec();
                    bad[at] ^= 0x80;
                    let r = open(&key, &nonce, &aad, &mut bad, t.try_into().unwrap());
                    assert_eq!(r, Err(MAC), "{name}: {n} bytes, byte {at}");
                }
                let mut bad_tag: [u8; TAG] = t.try_into().unwrap();
                bad_tag[15] ^= 1;
                let mut buf = ct.to_vec();
                assert_eq!(open(&key, &nonce, &aad, &mut buf, &bad_tag), Err(MAC));
                let mut buf = ct.to_vec();
                let mut aad2 = aad.clone();
                aad2.push(0);
                let r = open(&key, &nonce, &aad2, &mut buf, t.try_into().unwrap());
                assert_eq!(r, Err(MAC), "{name}: longer aad");
            }
        }
    }

    /// draft-irtf-cfrg-xchacha-03, A.3.1, through each backend.
    #[test]
    fn draft_vector() {
        let hex = |s: &str| purg_auth::hex::decode(s.as_bytes()).unwrap();
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 24] = hex("404142434445464748494a4b4c4d4e4f5051525354555657")
            .try_into()
            .unwrap();
        let ct = "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b4522f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e";
        for (name, seal, open) in backends() {
            let mut buf = plain.to_vec();
            let tag = seal(&key, &nonce, &aad, &mut buf);
            assert_eq!(purg_auth::hex::encode(&buf), ct, "{name}");
            assert_eq!(
                purg_auth::hex::encode(&tag),
                "c0875924c1c7987947deafd8780acf49",
                "{name}"
            );
            open(&key, &nonce, &aad, &mut buf, &tag).unwrap();
            assert_eq!(buf, plain, "{name}");
        }
    }

    /// HChaCha20 itself: draft-irtf-cfrg-xchacha-03, Section 2.2.1.
    #[cfg(feature = "ring")]
    #[test]
    fn hchacha_vector() {
        let hex = |s: &str| purg_auth::hex::decode(s.as_bytes()).unwrap();
        let key: [u8; 32] = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
            .try_into()
            .unwrap();
        let mut nonce = [0u8; NONCE];
        nonce[..16].copy_from_slice(&hex("000000090000004a0000000031415927"));
        nonce[16..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let (sub, n) = ring_backend::subkey(&key, &nonce);
        assert_eq!(
            purg_auth::hex::encode(&sub),
            "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc"
        );
        assert_eq!(n, [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
