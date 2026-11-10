//! Authentication from the guest side (`docs/auth.md`).
//!
//! [`crate::fauth`] gives an auth fd. The client reads the server's
//! challenge from it and writes a proof, then passes the fd to
//! [`crate::mount`]. [`amount`] does all of this, as Inferno's
//! `auth->amount` does.
//!
//! The proof comes from the first source that has a key: the guest
//! factotum (`/mnt/factotum/rpc`), `#¤` (and the certificate loaded into
//! it), an authinfo (a `-k` keyfile, else `/usr/<user>/keyring/default`),
//! then the seed in `/usr/<user>/key` or the public test key
//! `/lib/test/auth.key`.
//!
//! The formats and protocols themselves are in [`fmt`] (the `purg-auth`
//! crate); this module adds files, the namespace and randomness.

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub use purg_auth as fmt;
use purg_auth::cert::Cert;
use purg_auth::keys::AuthInfo;
use purg_auth::{PUBLIC, SEED, SIGNATURE};

use crate::{Error, abi};

/// Where factotum is mounted.
pub const FACTOTUM: &str = "/mnt/factotum";

/// Randomness from the kernel (WASI `random_get`), for keys, salts and
/// nonces.
pub struct Rng;

impl Rng {
    pub fn fill(buf: &mut [u8]) {
        random(buf)
    }

    /// A fresh Ed25519 seed.
    pub fn seed() -> [u8; SEED] {
        let mut s = [0u8; SEED];
        random(&mut s);
        s
    }
}

#[cfg(target_family = "wasm")]
fn random(buf: &mut [u8]) {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn random_get(buf: *mut u8, len: usize) -> i32;
    }
    // SAFETY: the host writes at most `buf.len()` bytes into `buf`.
    let r = unsafe { random_get(buf.as_mut_ptr(), buf.len()) };
    assert_eq!(r, 0, "random_get failed");
}

/// Host builds (clippy, unit tests) read the host's random device.
#[cfg(not(target_family = "wasm"))]
fn random(buf: &mut [u8]) {
    let mut f = std::fs::File::open("/dev/urandom").expect("/dev/urandom");
    f.read_exact(buf).expect("/dev/urandom");
}

impl rand_core::RngCore for Rng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        random(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        random(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        random(dest)
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        random(dest);
        Ok(())
    }
}

impl rand_core::CryptoRng for Rng {}

/// A [`fmt::login::Stream`] over a file: a connection or a pipe.
pub struct FileStream<'a>(pub &'a mut std::fs::File);

impl fmt::login::Stream for FileStream<'_> {
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), String> {
        self.0.read_exact(buf).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                String::from(abi::error::EHUNGUP)
            } else {
                Error::from(e).0
            }
        })
    }
    fn write_all(&mut self, buf: &[u8]) -> Result<(), String> {
        self.0.write_all(buf).map_err(|e| Error::from(e).0)
    }
}

/// This prog's user name, from `/dev/user`.
pub fn user() -> String {
    std::fs::read_to_string("/dev/user").unwrap_or_default()
}

/// The seconds since 1970 UTC.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn safe_user(user: &str) -> bool {
    !user.is_empty() && user != "." && user != ".." && !user.contains('/')
}

/// The path of authinfo `name`: itself when it holds a `/`, else
/// `/usr/<user>/keyring/<name>`.
pub fn keyfile_path(name: &str, user: &str) -> String {
    if name.contains('/') || name.starts_with('#') {
        String::from(name)
    } else {
        format!("/usr/{user}/keyring/{name}")
    }
}

/// Reads an authinfo file.
pub fn read_authinfo(path: &str) -> Result<AuthInfo, Error> {
    let text = std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    AuthInfo::parse(&text).map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

/// Reads a 32-byte seed file.
pub fn read_seed(path: &str) -> Result<[u8; SEED], Error> {
    let bytes = std::fs::read(path).map_err(|e| Error(format!("{path}: {}", Error::from(e))))?;
    <[u8; SEED]>::try_from(bytes.as_slice()).map_err(|_| Error(format!("{path}: not a key")))
}

/// Writes `data` to a new file only its owner can read.
pub fn write_secret(path: &str, data: &[u8]) -> Result<(), Error> {
    let mut f = crate::create(path, abi::mode::OWRITE | abi::mode::OTRUNC, 0o600)
        .map(std::fs::File::from)
        .map_err(|e| Error(format!("{path}: {}", e)))?;
    f.write_all(data)
        .map_err(|e| Error(format!("{path}: {}", Error::from(e))))
}

/// Reads one line from standard input (a password), without its newline.
/// Returns `None` at end of file.
pub fn read_line() -> Option<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    let mut stdin = std::io::stdin();
    loop {
        match stdin.read(&mut b) {
            Ok(1) if b[0] == b'\n' => break,
            Ok(1) => line.push(b[0]),
            _ if line.is_empty() => return None,
            _ => break,
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).ok()
}

/// Asks the guest factotum for a v2 proof. `None` when it is not mounted
/// or has no key.
fn factotum(challenge: &[u8], user: &str, aname: &str) -> Option<Vec<u8>> {
    use purg_auth::attr::quote;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{FACTOTUM}/rpc"))
        .ok()?;
    let mut rpc = |req: &[u8]| -> Option<Vec<u8>> {
        f.write_all(req).ok()?;
        let mut buf = vec![0u8; 8192];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    let start = format!(
        "start proto=ed25519 role=client user={} aname={}",
        quote(user),
        quote(aname)
    );
    if rpc(start.as_bytes())? != b"ok" {
        return None;
    }
    let mut w = b"write ".to_vec();
    w.extend_from_slice(challenge);
    if rpc(&w)? != b"ok" {
        return None;
    }
    rpc(b"read")?.strip_prefix(b"ok ").map(<[u8]>::to_vec)
}

/// Asks `#¤/rpc` to sign. `None` when the agent has no key.
fn agent_sign(msg: &[u8]) -> Option<[u8; SIGNATURE]> {
    let mut f = std::fs::File::from(crate::open("#¤/rpc", abi::mode::ORDWR).ok()?);
    f.write_all(msg).ok()?;
    let mut out = [0u8; abi::auth::PROOF];
    (f.read(&mut out).ok()? == out.len()).then(|| out[..SIGNATURE].try_into().ok())?
}

/// `#¤`'s public key and certificate, when it holds a key.
fn agent_key() -> Option<([u8; PUBLIC], Option<Cert>)> {
    let ctl = std::fs::read("#¤/ctl").ok()?;
    let hex = ctl.strip_prefix(b"key ")?.get(..PUBLIC * 2)?;
    let public = purg_auth::hex::array::<PUBLIC>(hex)?;
    Some((public, purg_auth::keys::find_cert(&ctl)))
}

/// Builds the proof for `challenge` as `user`, for attach name `aname`.
/// `keyfile` names an authinfo to use instead of the default sources.
pub fn proof_for(
    challenge: &[u8],
    user: &str,
    aname: &str,
    keyfile: Option<&str>,
) -> Result<Vec<u8>, Error> {
    if let Some(k) = keyfile {
        let info = read_authinfo(&keyfile_path(k, user))?;
        return Ok(purg_auth::proof::make(
            &info.seed,
            Some(&info.cert),
            challenge,
            user,
            aname,
        ));
    }
    if let Some(p) = factotum(challenge, user, aname) {
        return Ok(p);
    }
    if let Some((public, cert)) = agent_key() {
        let cert = cert.filter(|c| c.key == public);
        let (kind, body) = purg_auth::proof::body(cert.as_ref(), &public);
        let msg = purg_auth::proof::message(challenge, user, aname, kind, &body);
        if let Some(sig) = agent_sign(&msg) {
            return Ok(purg_auth::proof::assemble(kind, &sig, &body));
        }
    }
    if safe_user(user)
        && let Ok(info) = read_authinfo(&format!("/usr/{user}/keyring/default"))
    {
        return Ok(purg_auth::proof::make(
            &info.seed,
            Some(&info.cert),
            challenge,
            user,
            aname,
        ));
    }
    let seed = seed(user)?;
    Ok(purg_auth::proof::make(&seed, None, challenge, user, aname))
}

/// Signs `challenge` with no attach name and the default sources.
pub fn proof(challenge: &[u8], user: &str) -> Result<Vec<u8>, Error> {
    proof_for(challenge, user, "", None)
}

/// The signing seed: `/usr/<user>/key`, then the shared test fixture.
fn seed(user: &str) -> Result<[u8; SEED], Error> {
    let mut paths = Vec::new();
    if safe_user(user) {
        paths.push(format!("/usr/{user}/key"));
    }
    paths.push(String::from("/lib/test/auth.key"));
    for path in paths {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(seed) = <[u8; SEED]>::try_from(bytes.as_slice())
        {
            return Ok(seed);
        }
    }
    Err(Error::from(abi::auth::NO_KEY))
}

/// Runs the exchange on an auth fd from [`crate::fauth`]: reads the
/// challenge and writes the proof.
pub fn authenticate(afd: &OwnedFd, user: &str) -> Result<(), Error> {
    authenticate_for(afd, user, "", None)
}

/// [`authenticate`] for attach name `aname`, with an optional authinfo.
pub fn authenticate_for(
    afd: &OwnedFd,
    user: &str,
    aname: &str,
    keyfile: Option<&str>,
) -> Result<(), Error> {
    // SAFETY: `afd` stays open for this call, and ManuallyDrop keeps the
    // File from closing it.
    let mut f = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(afd.as_raw_fd()) });
    let mut challenge = [0u8; abi::auth::CHALLENGE];
    f.read_exact(&mut challenge)?;
    let p = proof_for(&challenge, user, aname, keyfile)?;
    // The proof is one write.
    let n = f.write(&p)?;
    if n != p.len() {
        return Err(Error::from(abi::auth::FAILED));
    }
    Ok(())
}

/// Inferno's `amount`: mount the 9P conversation on `fd` at `old`,
/// authenticating first when the server asks. With `abi::mount::MNOAUTH`
/// in `flags` there is no `Tauth` at all.
pub fn amount(fd: i32, old: &str, flags: u32, aname: &str) -> Result<(), Error> {
    amount_key(fd, old, flags, aname, None)
}

/// [`amount`] with an authinfo `keyfile` (`-k` on `mount` and `import`).
pub fn amount_key(
    fd: i32,
    old: &str,
    flags: u32,
    aname: &str,
    keyfile: Option<&str>,
) -> Result<(), Error> {
    if flags & abi::mount::MNOAUTH != 0 {
        return crate::mount(fd, -1, old, flags, aname);
    }
    match crate::fauth(fd, aname) {
        Ok(afd) => {
            authenticate_for(&afd, &user(), aname, keyfile)?;
            crate::mount(fd, afd.as_raw_fd(), old, flags, aname)
        }
        Err(e) if e.0 == abi::auth::NOT_REQUIRED => {
            crate::mount(fd, -1, old, flags | abi::mount::MNOAUTH, aname)
        }
        Err(e) => Err(e),
    }
}
