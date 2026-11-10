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
  (It now reads them in its own namespace instead; see below.)
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
  with `#` are ignored. Used for `/lib/keys/signers` and `/lib/keys/revoked`.

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

In every mode, a key listed in `/lib/keys/revoked` is refused: the user's
key, or a signer's key (whose signatures then do not count).

The `Tattach` that follows must use the same `uname` and `aname` as the
`Tauth`, or it fails with `authentication required`.

### Where the server looks

Everything is read in the **exporter's own namespace**, at absolute paths,
and never from under the exported tree:

- `/usr/<uname>/key.pub` and `/lib/test/auth.key.pub`: directly held keys.
- `/lib/keys/signers`: trusted signers (a key list).
- `/lib/keys/revoked`: revoked keys (a key list). A line that does not
  parse, or a list that is there but cannot be read, makes every
  certificate fail, so a damaged list fails closed.

The exporter's namespace is the namespace of whatever runs the export: the
prog that calls `export` (the `export` command, usually started by
`styxlisten` or `listen`, which share their namespace with it), or the
namespace the probe and the kernel pass to their built-in export. The files
are read at each `Tauth`, so a `bind` over `/lib/keys` in that namespace
changes what the server trusts from the next mount on. For `export /` the
two places are the same files, so that case behaves as it did when the
server looked under the tree. Someone who can write into an exported
subtree cannot add a key, a signer or a revocation.

A server with no directly held key and no trusted signer in its namespace
answers `Tauth` with `authentication not required`, as before. Note that
the stock root holds the public test key `/lib/test/auth.key.pub`, so an
export from an unchanged namespace asks for a proof whatever it exports,
and any client holding the (public) test seed passes; deployments remove
or hide that pair. `-A` (`MNOAUTH`) still
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

The `mount` command lets the kernel authenticate unless it is given `-k`
or a guest factotum is mounted; then it uses the SDK, as `import` always
does. The kernel's own mount (`mount` with `afd = -1`) and the SDK
(`purgatorio::auth`, used by `mount`, `import` and `amount`) build the
proof from the authinfo named by `-k keyfile` on `mount` or `import`, or
else from the first source that has a key:

1. the guest factotum, `/mnt/factotum/rpc` (SDK only);
2. `#¤`: its seed, plus the certificate loaded into it with `cert ...`;
3. the authinfo `/usr/<user>/keyring/default`;
4. `/usr/<user>/key`, then `/lib/test/auth.key` (direct key, kind 0).

The ABI version goes from 0.5 to 0.6, since a guest can now write a v2
proof to an auth fd and needs a kernel that reads it. Guests built for 0.5
keep working against a 0.6 server through the 96-byte form, but the loader
refuses mismatched modules anyway.

## Mount handshake, version 3

Version 2 authenticates the client and nothing more: the Styx conversation
after the attach is in the clear, and whoever can rewrite the connection
can take it over. Version 3 adds an ephemeral X25519 exchange, bound to the
proof, and protects everything after the proof with a record layer, as
Inferno pushes `ssl` after its handshake. It is a new version because v2's
format has no room for the client's key, and v2's signature does not cover
the server's.

### Message flow

```
C -> S  Tversion, Tauth afid uname aname          (clear)
S -> C  Rversion, Rauth
C -> S  Tread afid 0 64
S -> C  Rread  offer = challenge[32] || S[32]     S: server ephemeral key
        (the client makes c, its ephemeral key, C = X25519(c, base))
C -> S  Twrite afid 0  proof v3                    last clear request
S -> C  Rwrite  (or Rerror)                        last clear reply
        records from here on, each way
C -> S  Tattach fid afid uname aname, ...
```

```
proof v3 = 0x03 || kind u8 || signature[64] || C[32] || body
signed   = challenge[32] || S[32] || C[32]
message  = "pgauth3" 0x00 || signed || len(uname) u16 LE || uname
                          || len(aname) u16 LE || aname || kind u8 || body
```

`kind` and `body` are as in version 2: a public key (0) or a `pgcert1`
line (1). The signature is by the body's key over `message`, so it binds
the challenge, both ephemeral keys, both names and the client's identity.
The server checks it exactly as a version 2 proof. A server or client key
changed on the way makes it fail with `authentication failed`.

Keys:

```
shared = X25519(s, C) = X25519(c, S)              all zeros is refused
prk    = HKDF-Extract(salt = SHA-256(message), ikm = shared)
c2s    = HKDF-Expand(prk, "pgauth3 c2s", 32)
s2c    = HKDF-Expand(prk, "pgauth3 s2c", 32)
```

Both ephemeral secrets are random per `Tauth` and are dropped with the
exchange, so a recorded conversation stays unreadable even to someone who
later gets the user's long-term key.

### Where the records start

Each direction switches at one message, so both ends agree without an
extra round trip:

- The client seals everything it writes after the version 3 proof's
  `Twrite`, and opens everything it reads after the reply to that tag.
- The server opens everything it reads after a `Twrite` of a version 3
  proof to one of its auth fids (it needs only `C` from the proof, not the
  check), and seals every reply queued after the reply to that tag.

The switch happens whether the proof is accepted or not: a refused proof's
`Rerror` arrives in the clear, and the conversation is over. A client may
not send anything else while the proof is outstanding, and does not flush
it: a prog killed while it waits takes the whole session down instead.
A conversation switches once; a later proof on it is checked but changes
no keys.

### Record layer

```
record = size[4] || ciphertext || tag[16]
nonce  = seq u64 LE || 0[4]
aad    = size[4]
```

`size` (u32 LE) counts the whole record, header included, as a Styx
message's `size` does. Each direction has its own key and counts its
records from 0; the sequence number is never sent. One Styx message goes
in each record, so the largest record is the negotiated msize plus 20
bytes, and a larger `size` is refused before anything is read.

A record that fails to open (changed, replayed, reordered, dropped, or
under the other direction's key) fails with `bad record mac`, and so does
every record after it. A stream that ends inside a record is a hangup
(`i/o on hungup channel`). On the client every outstanding and later
request on the mount fails with that error; the server drops the
connection, and its `export` returns the error.

The record layer is in `purg_auth::record`, the exchange in
`purg_auth::session`. In the kernel, `#M` holds the client's sealer under
its write lock and the reader kproc holds the opener. `export` keeps its
opener in the reading thread; its replies go through the reply queue, and
only the thread draining that queue seals them, so concurrent workers'
replies go out as records in sequence order.

### Cost

Measured on an Apple Silicon host under Linux, release builds, with other
work running:

- The kernel's `.text` (`--profile kernel`) grows by 36 KiB, from 2,014
  to 2,050 KiB (1.8%). No crate is new; the X25519 and record code is
  most of it.
- Sealing and opening one 8 KiB record takes about 62 µs together (about
  130 MB/s); a 30-byte record about 1.3 µs. The exchange (two key pairs,
  two X25519, one Ed25519 signature, the key derivation) takes about
  0.7 ms.
- Reading 16 MiB through a mount over an in-memory pipe takes 205 ms
  encrypted against 68 ms in the clear. That is the worst case: with no
  network in the way, the cipher is all the cost there is.

### Compatibility and downgrade

- A server always offers 64 bytes. A version 1 or 2 client reads only the
  first 32, the challenge, and its conversation stays in the clear, as
  before.
- A version 3 client refuses a server whose offer is only a challenge,
  with `no session key offered`. Otherwise someone in the middle could cut
  the offer to 32 bytes and keep the conversation in the clear.
- The server still takes version 1 and 2 proofs, so guests built before
  this keep working. A server option to refuse them is not done yet.
- `-A` and a server that answers `authentication not required` stay in the
  clear.

There is **no encrypt-only mode** (records without identities). Without a
key to sign with, an exchange cannot tell the real server from a man in
the middle, so it would stop only passive eavesdroppers while looking like
protection; a server that asks for no authentication lets anyone attach
anyway; and it would be one more mode to review. Inferno likewise pushes
`ssl` only with the secret its authentication agreed, and `-A` leaves the
conversation in the clear. A user who wants an encrypted conversation
gets a key (`getauthinfo`, or the test key) and authenticates.

### Server authentication, later

The flow leaves room for the server to prove itself without a new
version. After the proof, the auth fid reads at offset 64 as the server's
own proof. Today that read is empty. A server with a key would return
`kind || signature[64] || body`, with the signature over
`"pgauth3 server" 0x00 || SHA-256(message)`, which binds its identity to
both ephemeral keys and the client's proof. That read already travels in
the records. A client that wants mutual authentication reads it and checks
it against its trusted signers; one that does not never asks.

### Clients and the ABI

The kernel's own mount (`afd = -1`) does all of the above itself. Through
`fauth`, a read of 96 bytes (`SIGNED`) from the auth fd returns the offer
followed by `C`; the kernel made the key and keeps `c`. The guest (or its
factotum, which builds the message itself from those 96 bytes) signs and
writes the proof; the kernel recognises it as the proof for its key and
switches the conversation. A read of 32 bytes still gives the bare
challenge. The ABI goes from 0.6 to 0.7: a guest built for 0.7 reads 96
bytes and refuses a kernel that does not answer with them.

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
| `auth/createsignerkey [-f keyfile] [name]` | [createsignerkey(8)] Makes a signer seed (default `/keydb/signerkey`) and prints its public key as a key-list line `<hex> <name>`, ready to append to `/lib/keys/signers`. |
| `auth/getpk file ...` | [getpk(8)] Prints the public key of a seed file, a public key file (named `*.pub`), an authinfo or a certificate, in hex. |
| `auth/verify [-s signers] [-r revoked] [-u name] [-t time] file` | [verify(8)] Checks the certificate in a file (a certificate or an authinfo) against a signer list and a revocation list, and prints its name, key, expiry and signers. Exits with the error a server would give. With `-m` it checks a module's signatures instead (`docs/signing.md`). |
| `auth/sign`, `auth/modpolicy` | Sign a module, and run a command under a module policy. See `docs/signing.md`. |
| `auth/mkauthinfo [-f signerkey] [-e expires] user keyfile` | [mkauthinfo(8)] Makes a user key and a certificate on the signer's machine and writes an authinfo. |
| `auth/countersigner [-f key] file` | [countersigner(8)] Adds this key's signature to the certificate in a file and prints the result. |
| `auth/keyfs [-m mntpt] [-C m,t] [keyfile]` | [keyfs(4)] Serves the key database (default `/keydb/keys` at `/mnt/keys`), sealed with ChaCha20-Poly1305 under an Argon2id key from the master password (read on standard input). Each user is a directory with `secret`, `expire` and `status`. `mkdir` adds a user, `rm` removes one, and every change rewrites the file. |
| `auth/changelogin [-m mntpt] [-C m,t] [-e expires] user` | [changelogin(8)] Run on the signer: reads a password, makes the verifier record and writes the user's entry in keyfs. |
| `auth/signer [-f signerkey] [-m mntpt] [-e days]` | [logind(8), signer(8)] The certificate server, for one conversation on fd 0 (run under `listen` or `styxlisten`). Checks the password with `pglogin1` and signs the client's key. The expiry is the account's, capped at `days` from now (default 365). |
| `getauthinfo [-s signer] [-u user] keyname` | [getauthinfo(8)] Reads the password, makes a key, gets it certified by the signer (a dial string, default `$signer`) and writes the authinfo `keyname` (a path, or a name under `/usr/<user>/keyring`). It checks that the certificate is for its key and name and is signed by the key the signer sent. |
| `auth/keysrv [-f signerkey] [-m mntpt]` | [keysrv(4)] The password-change server, for one conversation on fd 0. The signer key only salts the answers for unknown users. |
| `auth/passwd [-s server] [-u user]` | [passwd(1)] Reads the old password and the new one twice, and changes it through `keysrv` (default `$signer`). The new verifier is made on the client. |
| `auth/factotum [-m mntpt] [authinfo ...]` | [factotum(4)] A guest agent with Plan 9's `ctl` and `rpc` files (below). |
| `auth/secstore [-s dir] [-C m,t] [-g file \| -G file \| -p file \| -r file]` | [secstore(1)] Files encrypted under a password, kept in a store directory (default `/usr/<user>/secstore`, which can be an authenticated import). `-g` gets a file into the current directory, `-G` prints it, `-p` puts one, `-r` removes one, and no option lists the store. |
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
  `write <challenge>`, then `read` returns `ok <proof>`: a v3 proof when
  the challenge is the 96 bytes read from an auth fd (`SIGNED`), a v2
  proof for a bare 32-byte challenge. Factotum builds
  the whole signed message itself. It does not sign arbitrary bytes, so a
  malicious server cannot turn it into a signing oracle for other formats.
- `ed25519` with `role=modsign`: `write <digest[32] || abi>`, then `read`
  returns `ok <entry>`, a module signature entry (`docs/signing.md`).
  Factotum builds that message itself too.
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
  string (`pgcert1`, `pgauth2`, `pgauth3`), so a signature made for one purpose never
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
  certificate gains an attacker nothing. A version 3 client never falls
  back to version 2, so cutting the server's key out of the offer fails
  the mount instead of leaving it in the clear.
- **Session.** After a version 3 proof the conversation is encrypted and
  integrity-protected, and its keys come from ephemeral secrets (forward
  secrecy). Someone who can rewrite the connection can break it, which
  the client sees as `bad record mac`, but not read it or take it over.
  The server is not authenticated: a client that dials a fake server
  talks to the fake server, encrypted. What a man in the middle cannot do
  is relay the client's proof to the real server and keep the session,
  because the proof signs the real server's key. Conversations of older
  guests (version 1 and 2 proofs) are still in the clear and can be taken
  over after the attach.
- **Expiry.** Certificates carry an expiry, checked against the server's
  clock (`Platform::now_ns`; the probe and the kernel read the PL031).
  A server with a wrong clock accepts too much or too little. The signer
  caps expiry at the account's expiry and at `-e days`.
- **Revocation.** `/lib/keys/revoked` lists user or signer keys. It is
  read at each `Tauth`, so an edit takes effect on the next mount. A
  malformed list fails closed. There is no online status check.
- **Where trust comes from.** The server reads its keys, signers and
  revocations from its own namespace, not from the tree it exports, so a
  client that can write to an exported subtree cannot make itself trusted
  or revoke someone else. What remains: a client that can write the
  exporter's own `/lib/keys` or `/usr/*/key.pub` (through `export /`, say)
  can, so those files need permissions that keep clients out, and the
  exporter's namespace must not have an untrusted tree bound over `/lib`
  or `/usr`. The public test key in the stock root is trusted until it is
  removed.
- **Password guessing.** Online guessing is limited only by connection
  rate; the signer does not lock accounts. The stand-in answer for an
  unknown user uses the default Argon2id cost, so an account made with
  another `-C` cost can be told apart from a missing one. The stored verifier is
  password-equivalent (see the scheme above); keyfs is the defence.
- **Secrets in memory.** Seeds and verifiers are not zeroised after use.

## Implementation and tests

- `crates/purg-auth` (`no_std` + `alloc`): `cert`, `keys`, `attr`,
  `proof` (the handshake and the server's check), `session` (the version 3
  X25519 exchange and key derivation), `record` (the record layer) and,
  behind the default `login` feature, `login` (`pglogin1`, Argon2id,
  sealed files, `crypt`'s chunks). The kernel (`purg-core`) uses it
  without `login`. It builds for `aarch64-unknown-none` and
  `wasm32-wasip1`.
- Version 3 needs no new crate: X25519 is `curve25519-dalek` 4.1 (already
  under `ed25519-dalek`, BSD-3-Clause), and `chacha20poly1305` 0.10 and
  `hkdf` 0.12 are no longer behind `login`. All are pure Rust and
  `no_std`.
- New dependencies, all MIT OR Apache-2.0, pure Rust, `no_std`, used with
  default features off: `spake2` 0.4, `argon2` 0.5, `chacha20poly1305`
  0.10, `hkdf` 0.12, `hmac` 0.12. They share `curve25519-dalek`, `sha2`
  and `digest` with `ed25519-dalek`, which now has its `fast` feature (the
  basepoint tables, built at compile time) for quicker signing in guests.
- `purg-core`: `export.rs` reads the trust files per `Tauth` and checks
  proofs with `purg_auth::proof::check`; `auth.rs` builds the kernel
  mount's proof; `#¤` takes `cert`.
- The SDK's `purgatorio::auth` builds proofs, talks to factotum and `#¤`,
  and has the randomness, authinfo and stream helpers. `appl/authlib`
  holds what the tools share: keyfs accounts, the `pglogin1` server and
  client, and a small synthetic file server for keyfs and factotum.
- Version 3 tests: unit tests in `purg-auth` (the RFC 7748 X25519
  vector, both sides deriving the same keys, record vectors computed with
  a separate ChaCha20-Poly1305, and every changed bit, replay, reordering,
  truncation and size check); `purg-core`'s `styxcrypt` tests, which put a
  relay between a mount and an export that records both directions and
  can change a byte: no file name or data in the clear after the proof,
  `-A` and an unauthenticated server in the clear, a changed record each
  way, a changed server key and a stripped offer each failing the mount
  without a hang, `fauth` with a version 3 and a version 1 proof, and
  replies of concurrent workers staying in record order; the `m3` guest's
  `crypt_*` checks do the same through two relay progs on both VMs. Every
  authenticated mount in the sh corpus (`auth.sh` and others) now runs
  encrypted in the emu, the probe and the kernel.
- Tests: unit tests in `purg-auth` (formats, the proof check, the exchange
  over an in-memory pipe, sealing, a netkey test vector); `purg-core`'s
  `m3` test mounts with a certificate from an authinfo and from `#¤`, is
  refused once the key is revoked, and shows that a signer list, a key and
  a revocation planted inside the exported tree are ignored; the sh
  corpus's `auth.sh` runs the whole flow on loopback (keyfs, changelogin, signer, getauthinfo, an
  authenticated export with `/lib/keys` bound in the exporter's namespace
  and a planted signer list in the tree, expired, untrusted and revoked
  certificates, `-A`)
  on both VMs, in the probe and in the kernel; `/lib/test/auth/*.sh`
  (factotum, countersigning, keysrv and passwd, keyfs's sealed file, crypt,
  secstore, netkey) run on both VMs in the emu only, to keep the QEMU runs
  short.

## Decisions and open questions

Decided by the owner:

- **Trust files location.** The server reads `/lib/keys/*` and the user
  keys from the exporter's own namespace, not from under the exported tree
  (done; see "Where the server looks").
- **Session protection.** The handshake also agrees a session key (X25519
  next to the signature) and the conversation is encrypted and
  authenticated, as Inferno's `ssl` push does (done, issue #74; see
  "Mount handshake, version 3").
- **Module signing (#33)** reuses `pgcert1`, the key-list format and the
  revocation list. The design is in `docs/signing.md`.
- **Mutual authentication** (the server proving it holds a certificate
  from a signer the client trusts): maybe later. Version 3 leaves a place
  for it (see "Server authentication, later").

Still open:

1. **OPAQUE.** Move to an augmented PAKE once the RustCrypto crates for it
   are stable, so a stolen keyfs entry is not password-equivalent?
2. **Per-prog users.** `runas`, and servers that act for several users,
   need a user per prog rather than one per system.
