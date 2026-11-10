# Authentication tools

This is the design for Purgatorio's authentication tools (issue #38). It
follows plan decision D9: Ed25519 keys, a factotum-like agent, and modern
primitives instead of Inferno's keyring protocol. Inferno's man pages
describe the roles each tool plays. The protocols here are new, so real
Inferno peers still mount with `-A`.

## What exists before this work

- A mount authenticates before `Tattach`. The server sends a 32-byte
  challenge on the auth fid. The client writes `signature[64] ||
  public_key[32]`, an Ed25519 signature over the raw challenge.
- The server trusts any 32-byte public key it can read under the exported
  tree: `usr/<uname>/key.pub` and the test key `lib/test/auth.key.pub`.
  With no key it answers `Tauth` with `authentication not required`.
- The client signs through `#¤/rpc` (a kernel agent holding one seed) or
  with a seed file: `/usr/<user>/key`, then `/lib/test/auth.key`.
- `MNOAUTH` (`-A`) skips `Tauth`.

Every server must therefore hold the public key of every user. There is no
expiry, no revocation, and no way to get a key without copying files.

## Model

A **signer** is an Ed25519 key pair whose public half servers trust. A
**certificate** is the signer's signature over a user name, the user's
public key and an expiry. A server that trusts a signer accepts any user
holding a valid certificate from it. The direct-key mode stays: a key a
server holds itself is still accepted, with or without a certificate.

Users get certificates in two ways:

- **Online**, with a password: `getauthinfo` talks to the signer's network
  service (`auth/signer`). The signer checks the password against its key
  database (`auth/keyfs`) with a PAKE, so the password never crosses the
  network in any form an eavesdropper or a fake signer can test offline.
- **Offline**, by the operator: `auth/mkauthinfo` makes a key and a
  certificate on the signer's machine, as Inferno's `mkauthinfo` does.

The private user key never leaves the client in the online flow. The
client generates it and sends only the public half to be certified.

## Formats

All keys are Ed25519. Hex is lowercase. Times are seconds since
1970-01-01 UTC.

### Key files

- **Seed file:** the 32 raw bytes of an Ed25519 seed. This is the existing
  format of `/usr/<user>/key` and `/lib/test/auth.key`, and the format of
  the signer key `/keydb/signerkey`. Mode 0600.
- **Public key file:** 32 raw bytes, as `usr/<user>/key.pub` is now.
- **Key list:** text, one key per line: 64 hex digits, optionally followed
  by a space and a comment (usually a name). Blank lines and lines starting
  with `#` are ignored. Used for `lib/keys/signers` and `lib/keys/revoked`.

### Certificate (`pgcert1`)

One line of text, so it fits in a file, a factotum key attribute or a
shell variable:

```
pgcert1 <name> <key> <expires> <signer>:<signature> [<signer>:<signature> ...]
```

- `name`: the user name. 1 to 255 bytes of UTF-8 with no white space,
  control characters, `/` or `:`, and not `.` or `..`.
- `key`: the user's public key, 64 hex digits.
- `expires`: decimal seconds. `0` means it does not expire.
- each `signer:signature`: a signer's public key (64 hex digits) and its
  Ed25519 signature (128 hex digits) over the certificate body:

  ```
  "pgcert1" 0x00 || len(name) u8 || name || key[32] || expires u64 LE
  ```

At least one and at most 8 signatures, no signer twice. Parsing is strict:
the line must be exactly what the encoder writes, so a certificate has one
encoding. The version is in the first word. A later format gets a new word
(`pgcert2`) and old parsers reject it.

A countersignature is one more `signer:signature` over the same body.

### Authinfo

What `getauthinfo` and `mkauthinfo` write. It plays the part of Inferno's
`/usr/<user>/keyring/<keyname>` file and lives at the same path (default
keyname `default`). Text, three lines, mode 0600:

```
key <seed, 64 hex digits>
signer <the issuing signer's public key>
pgcert1 ...
```

The `signer` line tells the holder which signer it got its certificate
from, so it can trust that signer for peers too.

## Mount handshake, version 2

The server side is unchanged up to the challenge: `Tauth`, then a read of
the auth fid gives `challenge[32]`. The client then writes the proof in
**one write**:

- **Exactly 96 bytes:** a version 1 proof, `signature[64] ||
  public_key[32]`, signed over the raw challenge. Accepted only for keys
  the server holds directly. Kept for guests built before this change.
- **Anything else:** a versioned proof. The first byte is the version (2).
  No later version may be 96 bytes long.

```
proof v2 = 0x02 || kind u8 || signature[64] || body
kind 0: body = public_key[32]               (direct key)
kind 1: body = a pgcert1 line (no newline)  (certificate)
```

The signature is by the key in the body (the certificate's `key` for kind
1) over:

```
"pgauth2" 0x00 || challenge[32] || len(uname) u16 LE || uname
               || len(aname) u16 LE || aname || kind u8 || body
```

`uname` and `aname` are the ones the client sent in `Tauth`.

The server accepts a v2 proof when the signature verifies and:

- kind 0: the key is one it holds directly;
- kind 1: the key is one it holds directly, or the certificate's `name` is
  the `Tauth` uname, it has not expired (`expires` is 0 or later than
  now), and at least one of its signatures is by a trusted signer and
  verifies.

In every mode, a key listed in `lib/keys/revoked` is refused: the user's
key, or a signer's key (whose signatures then do not count).

The `Tattach` that follows must use the same `uname` and `aname` as the
`Tauth`, or it fails with `authentication required`.

### Where the server looks

As today, everything is read under the exported tree (for `export /` that
is the exporter's own `/lib/keys/...`):

- `usr/<uname>/key.pub` and `lib/test/auth.key.pub`: directly held keys.
- `lib/keys/signers`: trusted signers (a key list).
- `lib/keys/revoked`: revoked keys (a key list). A line that does not
  parse makes every certificate fail, so a damaged list fails closed.

A server with no directly held key and no trusted signer answers `Tauth`
with `authentication not required`, as before. `-A` (`MNOAUTH`) still
skips `Tauth`, and a server that authenticates still rejects such an
attach with `authentication required`.

### Errors

A rejected proof fails the write with one of:

| error | meaning |
|---|---|
| `authentication failed` | bad signature, malformed proof, or a key the server does not know |
| `certificate expired` | the certificate's expiry has passed |
| `certificate revoked` | the user's key, or the only trusted signer's key, is revoked |
| `untrusted signer` | no signature is by a signer the server trusts |
| `certificate name mismatch` | the certificate is for another user |

These say why a certificate was refused, which helps an operator and
tells an attacker nothing they do not already hold.

### Clients

The kernel's own mount (`mount` with `afd = -1`) and the SDK
(`purgatorio::auth`, used by `mount`, `import` and `amount`) build the
proof from the first source that has a key:

1. the guest factotum, `/mnt/factotum/rpc` (SDK only);
2. `#¤`: its seed, plus the certificate loaded into it with `cert ...`;
3. an authinfo: `-k keyfile` on `mount`/`import`, else
   `/usr/<user>/keyring/default`;
4. `/usr/<user>/key`, then `/lib/test/auth.key` (direct key, kind 0).

The ABI version goes from 0.5 to 0.6, since a guest can now write a v2
proof to an auth fd and needs a kernel that reads it. Guests built for 0.5
keep working against a 0.6 server through the 96-byte form, but the loader
refuses mismatched modules anyway.

## Password scheme

The signer must check a password without the password, or anything an
attacker can run a dictionary against, crossing the network. The choices:

1. **A password-derived key over TLS to a pinned signer key.** `#D` is a
   TLS 1.3 client that pins a P-256 key. There is no TLS server in the
   system, so the signer would need one. The client would also need the
   signer's key before its first login, and anyone holding the TLS key (or
   a fake signer, if pinning is skipped) receives a password hash to attack
   offline.
2. **An augmented PAKE (OPAQUE).** The server stores no
   password-equivalent. The pure-Rust `opaque-ke` is larger, depends on
   several pre-release crates of the new RustCrypto generation, and the
   key database is encrypted anyway.
3. **A balanced PAKE (SPAKE2)**, as Plan 9's dp9ik does with its
   SPAKE2-EE `authpak`. The server stores a password-derived value.

We use **SPAKE2** over the Ed25519 group (`spake2` 0.4 from RustCrypto
PAKEs), with the password first hardened by **Argon2id**:

- An eavesdropper learns nothing it can test a password against.
- A fake signer (or a man in the middle) gets one online guess per
  connection, and the client learns that the exchange failed.
- No key has to be distributed first. The signer's public key reaches the
  client inside the encrypted, password-authenticated reply, so
  `getauthinfo` can record it as a trusted signer.
- The cost: the stored verifier `w` is password-equivalent for this
  protocol. Whoever steals it can pose as the user to the signer or as the
  signer to the user. It is kept in `keyfs`, encrypted under the master
  key, and Argon2id makes recovering the password itself slow.

### Verifier

```
w = Argon2id(password, salt[16], m KiB, t passes, p lanes) -> 32 bytes
record = "argon2id <m> <t> <p> <salt hex> <w hex>"
```

`auth/changelogin` makes the record. The default cost is m = 19456 KiB,
t = 2, p = 1 (the OWASP baseline). `-C m,t` sets it, which the tests use to
stay fast under an interpreter in QEMU. The client takes the parameters
from the signer: a fake signer that sends weak ones gains nothing, because
it still has only one online guess.

### Login protocol (`pglogin1`)

Over any byte stream, usually TCP. Each frame is a u32 LE length (at most
16 KiB) and that many bytes.

```
C -> S  "pglogin1 <op> <name>"        op: cert (signer) or passwd (keysrv)
S -> C  "argon2id <m> <t> <p> <salt hex>"
S -> C  SPAKE2 message B (33 bytes)
C -> S  SPAKE2 message A (33 bytes)
C -> S  seal(c2s, request)
S -> C  seal(s2c, reply)   or   "error <reason>" in the clear
```

SPAKE2 runs with identities `A = <name>` and `B = "pgsigner"`, and gives a
32-byte key `K` that already binds both identities and both messages.
HKDF-SHA256 with `K`, salted with SHA-256 of the first two frames, derives
two ChaCha20-Poly1305 keys, one per direction (`pglogin1 c2s`, `pglogin1
s2c`). Nonces count from 0.

The server opens the client's sealed request before it reveals anything.
A failure there is `error authentication failed` in the clear, with no
detail. A request that opens proves the client knew the password, so the
server may then give a reason sealed (`account disabled`, `account
expired`).

- **cert:** the request is the user's public key in hex. The reply is
  `ok`, the signer's public key, and the certificate, one per line.
- **passwd:** the request is a new verifier record, computed by the client
  with the server's `m t p` and a fresh salt. The reply is `ok`.

For an unknown user the signer answers with a salt derived from its own key
and the name, and a random verifier, so the exchange looks the same and
fails at the sealed request. Probing for user names costs one connection
per guess and reveals nothing.

## Tools

Commands under `appl/cmd/auth/<name>` install as `/bin/auth/<name>.wasm`.
The Inferno page each one takes its role from is in brackets.

| tool | role |
|---|---|
| `auth/createsignerkey [-f keyfile] [name]` | [createsignerkey(8)] Makes a signer seed (default `/keydb/signerkey`) and prints its public key as a key-list line `<hex> <name>`, ready to append to `lib/keys/signers`. |
| `auth/getpk [file ...]` | [getpk(8)] Prints the public key of a seed file, a public key file or an authinfo, in hex. |
| `auth/verify [-s signers] [-r revoked] [-u name] [-t time] file` | [verify(8)] Checks the certificate in a file (a certificate or an authinfo) against a signer list and a revocation list, and prints its name, key, expiry and signers. Exits with the error a server would give. |
| `auth/mkauthinfo [-f signerkey] [-e expires] user keyfile` | [mkauthinfo(8)] Makes a user key and a certificate on the signer's machine and writes an authinfo. |
| `auth/countersigner [-f key] file` | [countersigner(8)] Adds this key's signature to the certificate in a file and prints the result. |
| `auth/keyfs [-m mntpt] [-C m,t] [keyfile]` | [keyfs(4)] Serves the key database (default `/keydb/keys` at `/mnt/keys`), sealed with ChaCha20-Poly1305 under an Argon2id key from the master password (read on standard input). Each user is a directory with `secret`, `expire` and `status`. `mkdir` adds a user, `rm` removes one, and every change rewrites the file. |
| `auth/changelogin [-m mntpt] [-C m,t] [-e expires] user` | [changelogin(8)] Run on the signer: reads a password, makes the verifier record and writes the user's entry in keyfs. |
| `auth/signer [-f signerkey] [-m mntpt] [-e days]` | [logind(8), signer(8)] The certificate server, for one conversation on fd 0 (run under `listen` or `styxlisten`). Checks the password with `pglogin1` and signs the client's key. The expiry is the account's, capped at `days` from now (default 365). |
| `getauthinfo [-u user] [-e days] signer keyname` | [getauthinfo(8)] Reads the password, makes a key, gets it certified by `signer` (a dial string) and writes the authinfo `keyname` (a path, or a name under `/usr/<user>/keyring`). |
| `auth/keysrv [-m mntpt]` | [keysrv(4)] The password-change server, for one conversation on fd 0. |
| `auth/passwd [-u user] server` | [passwd(1)] Reads the old password and the new one twice, and changes it through `keysrv`. |
| `auth/factotum [-m mntpt] [authinfo ...]` | [factotum(4)] A guest agent with Plan 9's `ctl` and `rpc` files (below). |
| `auth/secstore [-s dir] [-C m,t] [-g file \| -G file \| -p file \| -r file]` | [secstore(1)] Files encrypted under a password, kept in a store directory (default `/usr/<user>/secstore`, which can be an authenticated import). Without an option it lists the store. |
| `crypt [-d] [-C m,t] (-k secret \| -f secretfile)` | [crypt(1)] Encrypts or decrypts standard input to standard output with ChaCha20-Poly1305 in 64 KiB chunks, under an Argon2id key. |
| `netkey` | [netkey(1)] Reads a password, then challenges, and prints each response: the first 8 hex digits of HMAC-SHA256 keyed with SHA-256 of the password. It replaces the DES SecureNet calculator; it is only useful with a server that computes the same. |

### Merged, dropped or deferred

- **`logind`** is merged into `auth/signer`. In Inferno, `logind` issued
  certificates after a password check and `signer` issued them to keys an
  operator confirmed out of band, comparing a displayed "box" with
  `verify`. Here the password path is the only network path, and the
  operator path is `mkauthinfo`, run on the signer's machine. One network
  protocol is easier to review.
- **`verify`** keeps the name and checks a certificate, since there is no
  pending "box" to confirm.
- **`spki/verify`** is dropped. It checked SPKI S-expression certificates,
  which nothing here uses.
- **`runas`** is deferred. It needs a per-prog user, and the kernel has one
  user for the whole system (`Procs::user`). It becomes possible when
  progs carry their own user.
- **`auth(2)`'s `server` side** is the kernel's export. Only the client side
  is in the SDK (`purgatorio::auth`).

### Factotum

`auth/factotum` mounts at `/mnt/factotum` (`-m` to change it) with Plan 9's
files. Keys are written to `ctl` as attribute lists. An attribute starting
with `!` is secret and is never read back. Values may be quoted with `'`.

```
key proto=ed25519 [user=<name>] !seed=<hex> [cert='<pgcert1 line>']
key proto=pass server=<addr> user=<name> !password=<password>
delkey proto=<proto> [attr=value ...]
```

Reading `ctl` lists the keys, one per line, with secrets shown as
`!seed?`. Each authinfo named on the command line is loaded as an
`ed25519` key.

`rpc` speaks Plan 9's rpc: each write is a request, and the next read is
the reply (`ok [data]`, `done`, `needkey <attrs>`, `error <message>`).

- `ed25519`: `start proto=ed25519 role=client user=<u> aname=<a>`, then
  `write <challenge>`, then `read` returns `ok <v2 proof>`. Factotum builds
  the whole signed message itself. It does not sign arbitrary bytes, so a
  malicious server cannot turn it into a signing oracle for other formats.
- `pass`: `start proto=pass server=<s> [user=<u>]`, then `read` returns
  `ok <user> <password>`, both quoted.

`#¤` keeps working for existing callers: `ctl` still takes `key <hex>`, a
bare 32-byte seed or `delkey`, and now also `cert <pgcert1 line>`; `rpc`
still signs what is written to it. The SDK tries `/mnt/factotum` first.

## Security review

- **Replay.** The server's challenge is 32 fresh random bytes per `Tauth`,
  and the signature covers it, so a recorded proof is useless on another
  conversation. The login protocol's keys come from fresh SPAKE2 secrets.
- **Binding.** The v2 signature covers the `Tauth` uname and aname and the
  whole body. The attach must use the same uname and aname. A certificate
  for one user cannot attach as another.
- **Key confusion.** Every signed message starts with a distinct context
  string (`pgcert1`, `pgauth2`), so a signature made for one purpose never
  verifies for another. Two gaps remain, both from the version 1 path:
  `#¤/rpc` signs any bytes, and a version 1 proof signs a raw challenge
  the server chose. A hostile server can therefore get a signature over
  32 bytes of its choosing from a key in `#¤`. That cannot forge a
  certificate (a certificate body is longer than 32 bytes and starts with
  `pgcert1`) but the rule is still: **never load a signer key into `#¤`
  or factotum**; signer keys stay in seed files the signer reads. The guest
  factotum only signs `pgauth2` messages it builds itself.
- **Downgrade.** A v2 server still accepts v1 proofs, but only for keys it
  holds directly, which is exactly what v1 allowed before. Dropping the
  certificate gains an attacker nothing. Note that, as before, the Styx
  conversation after authentication is neither encrypted nor
  integrity-protected: someone who can rewrite the connection can take it
  over after the attach. Inferno pushes `ssl` with keys from its
  handshake; see the open questions.
- **Expiry.** Certificates carry an expiry, checked against the server's
  clock (`Platform::now_ns`; the probe and the kernel read the PL031).
  A server with a wrong clock accepts too much or too little. The signer
  caps expiry at the account's expiry and at `-e days`.
- **Revocation.** `lib/keys/revoked` lists user or signer keys. It is
  read at each `Tauth`, so an edit takes effect on the next mount. A
  malformed list fails closed. There is no online status check.
- **Trust files under the exported tree.** As with `key.pub` today, the
  server reads `lib/keys/*` under the tree it exports. Exporting a tree that
  its clients can write lets them add a signer. Export a subtree only when
  its `lib/keys` and `usr/*/key.pub` are not writable by its clients.
- **Password guessing.** Online guessing is limited only by connection
  rate; the signer does not lock accounts. The stored verifier is
  password-equivalent (see the scheme above); keyfs is the defence.
- **Secrets in memory.** Seeds and verifiers are not zeroised after use.

## Open questions for the owner

1. **Session protection.** Should the handshake also agree a session key
   (X25519 next to the signature) and push an encrypted, authenticated
   channel over the 9P conversation, as Inferno's `ssl` push does? This
   needs a TLS server or an `ssl`-like device.
2. **Mutual authentication.** Only the client proves who it is. Should the
   server also prove it holds a key with a certificate from a signer the
   client trusts (the authinfo's `signer` line)?
3. **Trust files location.** Read `lib/keys/*` from the exporter's own
   namespace (`/lib/keys`) instead of under the exported tree?
4. **OPAQUE.** Move to an augmented PAKE once the RustCrypto crates for it
   are stable, so a stolen keyfs entry is not password-equivalent?
5. **Per-prog users.** `runas`, and servers that act for several users,
   need a user per prog rather than one per system.
6. **Module signing (#33).** Reuse the certificate format, so a module's
   `purgatorio.sig` can carry a `pgcert1` for the signing key, and
   `lib/keys/signers` doubles as the module trust list?
