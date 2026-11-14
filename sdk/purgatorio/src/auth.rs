//! Authentication from the guest side (`docs/auth.md`).
//!
//! [`crate::fauth`] gives an auth fd. The client reads what to sign from
//! it (the server's challenge and ephemeral key, and the kernel's
//! ephemeral key for this client) and writes a version 3 proof, then
//! passes the fd to [`crate::mount`]. From the proof on the conversation is
//! encrypted. [`amount`] does all of this, as Inferno's `auth->amount`
//! does.
//!
//! The proof comes from the first source that has a key: the guest
//! factotum (`/mnt/factotum/rpc`), `#¤` (and the certificate loaded into
//! it), an authinfo (a `-k` keyfile, else `/usr/<user>/keyring/default`),
//! then the seed in `/usr/<user>/key` or the public test key
//! `/lib/test/auth.key`, which no stock server trusts (`docs/auth.md`,
//! "Trust by default").
//!
//! A server proves itself in turn (`docs/auth.md`, "Server
//! authentication"). For a mount the kernel checks that proof when the
//! mount has `abi::mount::MSRVAUTH`, and [`amount_host`] checks that it is
//! for the host dialled; [`check_server_host`] and [`server_proof`] are
//! for protocols the guest runs itself, as `rcmd` does.
//!
//! The formats and protocols themselves are in [`fmt`] (the `purg-auth`
//! crate); this module adds files, the namespace and randomness.

use std::io::{Read, Seek, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub use purg_auth as fmt;
use purg_auth::cert::Cert;
use purg_auth::keys::AuthInfo;
use purg_auth::modsig;
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

/// The classical key exchange (X25519), the default.
pub const KEX_CLASSICAL: &str = "x25519";
/// The hybrid post-quantum key exchange, X25519 with ML-KEM-768.
pub const KEX_PQ: &str = "x25519mlkem768";

/// The mount flags for `-C alg` on `mount`, `import` and `export`
/// (`docs/auth.md`, "Post-quantum options"): [`KEX_CLASSICAL`] adds
/// nothing, [`KEX_PQ`] adds `abi::mount::MPQ`, which requires the hybrid
/// exchange. Anything else is refused.
pub fn kex_flags(alg: &str) -> Result<u32, Error> {
    match alg {
        KEX_CLASSICAL => Ok(0),
        KEX_PQ => Ok(abi::mount::MPQ),
        _ => Err(Error(format!("unknown algorithm: {alg}"))),
    }
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

/// Whether a guest factotum is mounted at [`FACTOTUM`].
pub fn have_factotum() -> bool {
    crate::stat(&format!("{FACTOTUM}/rpc")).is_ok()
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
    // The write moved the offset, and the reply is read from its start.
    f.seek(std::io::SeekFrom::Start(0)).ok()?;
    let mut out = [0u8; abi::auth::PROOF];
    (f.read(&mut out).ok()? == out.len()).then(|| out[..SIGNATURE].try_into().ok())?
}

/// `#¤`'s public key and certificate, when it holds a key.
fn agent_key() -> Option<([u8; PUBLIC], Option<Cert>)> {
    // A device name is not a path wasi-libc can resolve: open it directly.
    let mut ctl = Vec::new();
    std::fs::File::from(crate::open("#¤/ctl", abi::mode::OREAD).ok()?)
        .read_to_end(&mut ctl)
        .ok()?;
    let hex = ctl.strip_prefix(b"key ")?.get(..PUBLIC * 2)?;
    let public = purg_auth::hex::array::<PUBLIC>(hex)?;
    Some((public, purg_auth::keys::find_cert(&ctl)))
}

/// Asks the guest factotum for a module signature entry
/// (`docs/signing.md`). `None` when it is not mounted or has no key.
fn factotum_module(digest: &[u8; modsig::DIGEST], abi: &[u8]) -> Option<modsig::Entry> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("{FACTOTUM}/rpc"))
        .ok()?;
    let mut rpc = |req: &[u8]| -> Option<Vec<u8>> {
        f.write_all(req).ok()?;
        let mut buf = vec![0u8; 16384];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        Some(buf)
    };
    if rpc(b"start proto=ed25519 role=modsign")? != b"ok" {
        return None;
    }
    let mut w = b"write ".to_vec();
    w.extend_from_slice(digest);
    w.extend_from_slice(abi);
    if rpc(&w)? != b"ok" {
        return None;
    }
    modsig::Entry::parse(rpc(b"read")?.strip_prefix(b"ok ")?)
}

/// Signs a module's digest and ABI version with a key held by an agent:
/// the guest factotum, then `#¤`. The private key never reaches this prog.
pub fn sign_module(digest: &[u8; modsig::DIGEST], abi: &[u8]) -> Result<modsig::Entry, Error> {
    if let Some(e) = factotum_module(digest, abi) {
        return Ok(e);
    }
    let (public, cert) = agent_key().ok_or_else(|| Error::from(abi::auth::NO_KEY))?;
    let msg = modsig::message(digest, abi).ok_or_else(|| Error::from(modsig::BAD_SIGNATURE))?;
    let sig = agent_sign(&msg).ok_or_else(|| Error::from(abi::auth::NO_KEY))?;
    let certs = cert.filter(|c| c.key == public).into_iter().collect();
    modsig::Entry::new(public, sig, certs).map_err(Error::from)
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
            return Ok(purg_auth::proof::assemble(challenge, kind, &sig, &body));
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

/// Asks the guest factotum for a server proof for the session `keys`
/// (`role=server`). `None` when it is not mounted or has no key.
fn factotum_server(keys: &purg_auth::session::Keys) -> Option<Vec<u8>> {
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
    if rpc(b"start proto=ed25519 role=server")? != b"ok" {
        return None;
    }
    let mut w = b"write ".to_vec();
    w.extend_from_slice(&keys.transcript);
    w.extend_from_slice(&keys.bind);
    if rpc(&w)? != b"ok" {
        return None;
    }
    let p = rpc(b"read")?.strip_prefix(b"ok ")?.to_vec();
    purg_auth::server::parse(&p)?;
    Some(p)
}

/// A server's proof (`purg_auth::server`) for the session `keys`, made as
/// `user` from the authinfo `keyfile` when given, else from the first
/// default source with a key: the guest factotum (`role=server`), `#¤`,
/// `/usr/<user>/keyring/default`, `/usr/<user>/key`, the test key. `None`
/// when there is no key.
pub fn server_proof(
    keys: &purg_auth::session::Keys,
    user: &str,
    keyfile: Option<&str>,
) -> Option<Vec<u8>> {
    use purg_auth::server;
    if let Some(k) = keyfile {
        let info = read_authinfo(&keyfile_path(k, user)).ok()?;
        return Some(server::make(&info.seed, Some(&info.cert), keys));
    }
    if let Some(p) = factotum_server(keys) {
        return Some(p);
    }
    if let Some((public, cert)) = agent_key() {
        let cert = cert.filter(|c| c.key == public);
        let (kind, body) = purg_auth::proof::body(cert.as_ref(), &public);
        if let Some(sig) = agent_sign(&server::message(keys, kind, &body)) {
            return Some(server::assemble(kind, &sig, &body));
        }
    }
    if safe_user(user)
        && let Ok(info) = read_authinfo(&format!("/usr/{user}/keyring/default"))
    {
        return Some(server::make(&info.seed, Some(&info.cert), keys));
    }
    let seed = seed(user).ok()?;
    Some(server::make(&seed, None, keys))
}

/// What this namespace trusts as a server: the keys `/lib/keys/servers`
/// lists for a host (or `*`), certificates from `/lib/keys/serversigners`
/// (or, when there is no such file, `/lib/keys/signers`), less
/// `/lib/keys/revoked` (a list that is there but does not parse fails
/// every certificate). The kernel's `MSRVAUTH` check reads the same files
/// but takes every key in `/lib/keys/servers`, whatever its host.
pub struct ServerTrust {
    pub servers: Vec<purg_auth::keys::Named>,
    pub signers: Vec<[u8; PUBLIC]>,
    pub revoked: Option<Vec<[u8; PUBLIC]>>,
}

impl ServerTrust {
    pub fn read() -> ServerTrust {
        use purg_auth::keys::{parse_list, parse_list_lenient, parse_named};
        let servers = std::fs::read("/lib/keys/servers")
            .map(|t| parse_named(&t))
            .unwrap_or_default();
        // Signers for servers only replace the shared list when there.
        let signers = match std::fs::read("/lib/keys/serversigners") {
            Ok(t) => parse_list_lenient(&t),
            Err(_) if crate::stat("/lib/keys/serversigners").is_ok() => Vec::new(),
            Err(_) => std::fs::read("/lib/keys/signers")
                .map(|t| parse_list_lenient(&t))
                .unwrap_or_default(),
        };
        let revoked = match std::fs::read("/lib/keys/revoked") {
            Ok(t) => parse_list(&t),
            Err(_) if crate::stat("/lib/keys/revoked").is_ok() => None,
            Err(_) => Some(Vec::new()),
        };
        ServerTrust {
            servers,
            signers,
            revoked,
        }
    }

    /// The server keys held for `host`; for `None`, every one.
    pub fn keys(&self, host: Option<&str>) -> Vec<[u8; PUBLIC]> {
        purg_auth::keys::keys_for(&self.servers, host)
    }

    /// The policy for `keys`, from [`ServerTrust::keys`].
    pub fn policy<'a>(&'a self, keys: &'a [[u8; PUBLIC]]) -> purg_auth::proof::Policy<'a> {
        purg_auth::proof::Policy {
            keys,
            signers: &self.signers,
            revoked: self.revoked.as_deref(),
            now: now(),
        }
    }
}

/// Checks a server's `proof` for the session `keys` against
/// [`ServerTrust`], for a server dialled at `host`: its key must be listed
/// for that host, or its certificate name it (`docs/auth.md`, "Servers
/// per host"). With no `host` any trusted server will do. Fails with
/// `abi::auth::SERVER_NONE` for an empty proof,
/// `abi::auth::NAME_MISMATCH` for a certificate for another host, and
/// `abi::auth::SERVER_FAILED` for anything else that does not pass.
pub fn check_server_host(
    keys: &purg_auth::session::Keys,
    proof: &[u8],
    host: Option<&str>,
) -> Result<(), Error> {
    let t = ServerTrust::read();
    let k = t.keys(host);
    purg_auth::server::check_host(&t.policy(&k), keys, proof, host).map_err(Error::from)
}

/// [`check_server_host`] with no host: any server this namespace trusts.
pub fn check_server(keys: &purg_auth::session::Keys, proof: &[u8]) -> Result<(), Error> {
    check_server_host(keys, proof, None)
}

/// Checks who the server behind the auth fd `afd` says it is, for a
/// server dialled at `host`, once the proof is written: reads its proof
/// from the auth fd (at `abi::auth::SERVER_PROOF_OFFSET`) and checks its
/// key or certificate against [`ServerTrust`] for `host`. The signature
/// over the session is the kernel's to check, which a mount with
/// `abi::mount::MSRVAUTH` does on the same bytes.
pub fn check_server_fd(afd: &impl AsRawFd, host: Option<&str>) -> Result<(), Error> {
    // SAFETY: `afd` stays open for this call, and ManuallyDrop keeps the
    // File from closing it.
    let mut f = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(afd.as_raw_fd()) });
    f.seek(std::io::SeekFrom::Start(abi::auth::SERVER_PROOF_OFFSET))?;
    let mut proof = vec![0u8; purg_auth::proof::MAX_LEN];
    let n = f.read(&mut proof)?;
    proof.truncate(n);
    let t = ServerTrust::read();
    let k = t.keys(host);
    purg_auth::server::check_identity(&t.policy(&k), &proof, host).map_err(Error::from)
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
    authenticate_with(afd, user, aname, keyfile, false)
}

/// [`authenticate_for`], with the post-quantum exchange of a version 4
/// proof when `pq` is set (`abi::mount::MPQ`, `docs/auth.md`,
/// "Post-quantum options"): a server that does not offer it fails with
/// `abi::auth::NO_PQ`.
pub fn authenticate_with(
    afd: &OwnedFd,
    user: &str,
    aname: &str,
    keyfile: Option<&str>,
    pq: bool,
) -> Result<(), Error> {
    // SAFETY: `afd` stays open for this call, and ManuallyDrop keeps the
    // File from closing it.
    let mut f = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(afd.as_raw_fd()) });
    // One read: the kernel answers a read of SIGNED bytes with the offer
    // and its own ephemeral key. Anything shorter is a server that offers
    // no session key, and is refused rather than spoken to in the clear.
    // A read of SIGNED_PQ bytes asks for the long offer, and the kernel
    // fails it when the server gives none.
    let want = if pq {
        abi::auth::SIGNED_PQ
    } else {
        abi::auth::SIGNED
    };
    let mut signed = vec![0u8; want];
    let n = f.read(&mut signed)?;
    if n != want {
        return Err(Error::from(if pq {
            abi::auth::NO_PQ
        } else {
            abi::auth::NO_SESSION
        }));
    }
    let p = proof_for(&signed, user, aname, keyfile)?;
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
/// With `abi::mount::MSRVAUTH` in `flags` the kernel checks the server's
/// proof before the attach, and a server that asks for no proof is
/// refused with `abi::auth::SERVER_NONE`.
pub fn amount_key(
    fd: i32,
    old: &str,
    flags: u32,
    aname: &str,
    keyfile: Option<&str>,
) -> Result<(), Error> {
    amount_host(fd, old, flags, aname, keyfile, None)
}

/// [`amount_key`] for a connection dialled at `host` (`mount -S` and
/// `import -S` with a dial string, from [`crate::dial::host`]): with
/// `abi::mount::MSRVAUTH` the server's proof must also be for that host
/// ([`check_server_fd`]) before the mount is made.
pub fn amount_host(
    fd: i32,
    old: &str,
    flags: u32,
    aname: &str,
    keyfile: Option<&str>,
    host: Option<&str>,
) -> Result<(), Error> {
    if flags & abi::mount::MNOAUTH != 0 {
        return crate::mount(fd, -1, old, flags, aname);
    }
    match crate::fauth(fd, aname) {
        Ok(afd) => {
            let pq = flags & abi::mount::MPQ != 0;
            authenticate_with(&afd, &user(), aname, keyfile, pq)?;
            if flags & abi::mount::MSRVAUTH != 0 && host.is_some() {
                check_server_fd(&afd, host)?;
            }
            crate::mount(fd, afd.as_raw_fd(), old, flags, aname)
        }
        Err(e) if e.0 == abi::auth::NOT_REQUIRED && flags & abi::mount::MSRVAUTH != 0 => {
            Err(Error::from(abi::auth::SERVER_NONE))
        }
        Err(e) if e.0 == abi::auth::NOT_REQUIRED && flags & abi::mount::MPQ != 0 => {
            Err(Error::from(abi::auth::NO_PQ))
        }
        Err(e) if e.0 == abi::auth::NOT_REQUIRED => {
            crate::mount(fd, -1, old, flags | abi::mount::MNOAUTH, aname)
        }
        Err(e) => Err(e),
    }
}
