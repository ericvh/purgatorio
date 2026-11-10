//! Mount authentication from the guest side.
//!
//! [`crate::fauth`] gives an auth fd. The client reads the server's
//! challenge from it and writes `signature || public_key`
//! ([`abi::auth::PROOF`] bytes), then passes the fd to [`crate::mount`].
//! [`amount`] does all of this, as Inferno's `auth->amount` does.
//!
//! The proof comes from `#¤/rpc` when the agent holds a key. Otherwise the
//! seed is read from `/usr/<user>/key`, or the public test key
//! `/lib/test/auth.key`, and signed here.

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use ed25519_dalek::{Signer, SigningKey};

use crate::{Error, abi};

/// Signs `challenge`, returning `signature || public_key`.
pub fn proof(challenge: &[u8], user: &str) -> Result<[u8; abi::auth::PROOF], Error> {
    if let Some(p) = factotum(challenge) {
        return Ok(p);
    }
    let seed = seed(user)?;
    let key = SigningKey::from_bytes(&seed);
    let mut out = [0u8; abi::auth::PROOF];
    out[..abi::auth::SIGNATURE].copy_from_slice(&key.sign(challenge).to_bytes());
    out[abi::auth::SIGNATURE..].copy_from_slice(&key.verifying_key().to_bytes());
    Ok(out)
}

/// Asks `#¤/rpc` to sign. `None` when the agent has no key.
fn factotum(challenge: &[u8]) -> Option<[u8; abi::auth::PROOF]> {
    let mut f = std::fs::File::from(crate::open("#¤/rpc", abi::mode::ORDWR).ok()?);
    f.write_all(challenge).ok()?;
    let mut out = [0u8; abi::auth::PROOF];
    (f.read(&mut out).ok()? == out.len()).then_some(out)
}

/// The signing seed: `/usr/<user>/key`, then the shared test fixture.
fn seed(user: &str) -> Result<[u8; abi::auth::SEED], Error> {
    let mut paths = Vec::new();
    if !user.is_empty() && user != "." && user != ".." && !user.contains('/') {
        paths.push(format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for path in paths {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(seed) = <[u8; abi::auth::SEED]>::try_from(bytes.as_slice())
        {
            return Ok(seed);
        }
    }
    Err(Error::from(abi::auth::NO_KEY))
}

/// This prog's user name, from `/dev/user`.
pub fn user() -> String {
    std::fs::read_to_string("/dev/user").unwrap_or_default()
}

/// Runs the exchange on an auth fd from [`crate::fauth`]: reads the
/// challenge and writes the proof.
pub fn authenticate(afd: &OwnedFd, user: &str) -> Result<(), Error> {
    // SAFETY: `afd` stays open for this call, and ManuallyDrop keeps the
    // File from closing it.
    let mut f = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(afd.as_raw_fd()) });
    let mut challenge = [0u8; abi::auth::CHALLENGE];
    f.read_exact(&mut challenge)?;
    let p = proof(&challenge, user)?;
    f.write_all(&p)?;
    Ok(())
}

/// Inferno's `amount`: mount the 9P conversation on `fd` at `old`,
/// authenticating first when the server asks. With `abi::mount::MNOAUTH`
/// in `flags` there is no `Tauth` at all.
pub fn amount(fd: i32, old: &str, flags: u32, aname: &str) -> Result<(), Error> {
    if flags & abi::mount::MNOAUTH != 0 {
        return crate::mount(fd, -1, old, flags, aname);
    }
    match crate::fauth(fd, aname) {
        Ok(afd) => {
            authenticate(&afd, &user())?;
            crate::mount(fd, afd.as_raw_fd(), old, flags, aname)
        }
        Err(e) if e.0 == abi::auth::NOT_REQUIRED => {
            crate::mount(fd, -1, old, flags | abi::mount::MNOAUTH, aname)
        }
        Err(e) => Err(e),
    }
}
