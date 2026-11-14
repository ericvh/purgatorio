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
- **Named key list:** text, one key per line, with the name it is trusted
  for first: `<name> <key> [comment]`. The name is a user (in
  `/lib/keys/users`) or a host (in `/lib/keys/servers`), compared with
  ASCII case ignored, or `*` for any. Blank lines and lines starting with
  `#` are ignored, and so is a line that does not parse: it grants nothing
  and does not cancel the others (`purg_auth::keys::parse_named`).

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

- `/usr/<uname>/key.pub`, and the keys `/lib/keys/users` (a named key
  list) gives for `uname` or `*`: directly held keys.
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
cannot authenticate anyone, so it lets nobody in: `Tauth` and `Tattach`
fail with `server trusts no keys` (see "Trust by default"). `-A`
(`MNOAUTH`) on the export asks for no proof and lets everyone in, and
answers `Tauth` with `authentication not required`. `-A` on the mount
skips `Tauth`, and a server that authenticates rejects such an attach
with `authentication required`.

### Trust by default

The stock root trusts **no key**. `/lib/keys/signers`, `/lib/keys/users`
and `/lib/keys/servers` are there with comments that say what goes in
them, and nothing else. The public test pair stays in `/lib/test`
(`auth.key`, `auth.key.pub`) because tests sign with it, but nothing reads
`auth.key.pub` as trusted any more. Tests that want it trusted bind
`/lib/test/keys` on `/lib/keys` in their own namespace: its `users` and
`servers` lists name the test key for `*`. A client with no key of its
own still falls back to proving with the test seed, which no stock server
accepts.

A server without `-A` that trusts nothing **fails closed**: an export
answers every `Tauth` and `Tattach` with `server trusts no keys`, and
`rstyxd` refuses every client with the same error. Before, such a server
answered `authentication not required` and let everyone in, so an
`export` or `rstyxd` started on a fresh system served its whole namespace
to anyone who could reach it. This is the owner's decision in issue #91
("tighten trust before mcpd gets real remote use"), and the reasoning is:

- **Inferno does the same.** An Inferno listener that authenticates
  (`listen` or `styxlisten` without `-A`) reads its keyring before it
  serves anyone, and with no keyring it cannot authenticate anyone, so it
  serves no one. A service meant for everyone says `-A`.
- **The kernel cannot tell local from remote.** An export only sees a fd.
  A pipe to a prog on the same system looks like a pipe to `rcmd`, which
  relays it to the network (the client's `/n/client` export). Opening
  pipes but closing connections would leave that hole, so every export
  without `-A` follows the same rule.
- **The cost is one flag.** An export to a prog on the same system that
  does not want authentication says so: `mount {export -A dir} /n/x`, as
  `mcpd` already did for its sessions' read-only exports. Nothing listens
  on the network without trust material or an explicit `-A`.

The kernel's and the probe's own exports (`netexport`, and the probe's
export for the host) and the kernel test mode's `rstyxd` exist only in
test mode, where the host connects as a test client. They run in a copy
of the boot namespace with `/lib/test/keys` bound on `/lib/keys`. The
interactive kernel and the corpus scripts run in the boot namespace,
which trusts nothing; a corpus script that needs the test key binds
`/lib/test/keys` itself.

### Setting up keys

Trust files live under `/lib/keys` in the namespace of the program that
checks: the exporter (`export`, `rstyxd`) for its clients, the mounter
(`mount -S`, `import -S`, `rcmd -S`) for its servers. Keep them writable
only by the administrator.

With a signer (one machine holds the signer key; servers and users get
certificates):

```sh
# On the signer's machine: make the signer key, trust it here.
auth/createsignerkey -f /keydb/signerkey mysigner >> /lib/keys/signers
# Copy that line into /lib/keys/signers on every machine that should
# accept the signer's certificates (clients and servers alike).

# A user's key and certificate, made on the signer's machine and copied
# to the user's /usr/<user>/keyring/default (mode 0600):
auth/mkauthinfo -f /keydb/signerkey alice /usr/alice/keyring/default
# or online, with a password checked by auth/signer, which the signer
# runs under styxlisten with auth/keyfs mounted (see "Tools"):
getauthinfo -s tcp!signer!infsigner default

# A server's certificate names the host clients dial (the host field of
# tcp!host!port, as they write it):
auth/mkauthinfo -f /keydb/signerkey fs.example /keydb/fs.example
styxlisten 'tcp!*!styx' export -k /keydb/fs.example / &
listen 'tcp!*!rstyx' /bin/auxi/rstyxd.wasm -k /keydb/fs.example &
```

With keys held directly (no signer), list the public keys:

```sh
auth/getpk /usr/alice/keyring/default        # prints the key in hex
echo alice <hex> >> /lib/keys/users           # on the server
echo fs.example <hex> >> /lib/keys/servers    # on the clients
```

`/usr/<user>/key.pub` (32 raw bytes) still works for a user, as before.
Revoke a key or a signer by adding its line (from `auth/getpk` or
`auth/createsignerkey`) to `/lib/keys/revoked`.

An export serves whatever is under its tree, so `export /` also serves
`/keydb` and `/lib/keys`. Export a subtree, or load the server's key into
the guest factotum (`auth/factotum /keydb/fs.example`, then remove the
file and start the export without `-k`): the export then asks factotum
for its proof and never reads the key (see "Factotum").

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

The `mount` command lets the kernel authenticate unless it is given `-k`,
a guest factotum is mounted, or `-S` with a dial string (whose host the
SDK checks, see "Servers per host"); then it uses the SDK, as `import`
always does. The kernel's own mount (`mount` with `afd = -1`) and the SDK
(`purgatorio::auth`, used by `mount`, `import` and `amount`) build the
proof from the authinfo named by `-k keyfile` on `mount` or `import`, or
else from the first source that has a key:

1. the guest factotum, `/mnt/factotum/rpc` (SDK only);
2. `#¤`: its seed, plus the certificate loaded into it with `cert ...`;
3. the authinfo `/usr/<user>/keyring/default`;
4. `/usr/<user>/key`, then `/lib/test/auth.key` (direct key, kind 0),
   which no stock server trusts.

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
`Rerror` arrives in the clear, and the conversation is over. The client
does not flush the proof: a prog killed while it waits takes the whole
session down instead. A conversation switches once; a later proof on it
is checked but changes no keys.

The kernel enforces the rest:

- **One key per auth fd.** The first read of the offer makes the
  client's ephemeral key, and every later read, from any thread, returns
  the same 96 bytes. A read beside the write of the proof therefore cannot
  change the key under it. The proof itself is written under the
  session's write lock, which also installs the sealer, so a request from
  another thread goes either wholly before the proof (in the clear) or
  wholly after it (sealed), and both ends switch at the same message.
  Replies switch at the proof's reply in stream order on both ends, so a
  reply to a request from another thread that overtakes or follows the
  proof's reply is read the way it was sent. Nothing more is needed.
- **No restart in the clear.** The device remembers every conversation
  (by its fd) that went to records, even through a refused proof and
  after its auth fd is closed. A `mount` with `afd = -1` or a `fauth` on
  it fails with `conversation is encrypted`, rather than sending a
  `Tversion` the server would read as a broken record.

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

Each direction carries at most 2^48 records (`purg_auth::record::LIMIT`).
The record after that fails to seal, and one that arrives past it fails
to open, with `record sequence exhausted`, which ends the conversation as
a bad record does. There is no rekeying. The 64-bit nonce could count to
2^64 without repeating; the limit is a clean error far short of that, so
no arithmetic near the counter's end is ever used. At a million records
a second, 2^48 takes nine years; a 2^64 count is unreachable in practice,
and a conversation that long can dial again. ChaCha20-Poly1305 has no
per-key data limit that comes into play before then: its integrity bound
concerns forgery attempts, and here the first failed record ends the
conversation.

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
- The hosted emu now uses `ring`'s ChaCha20-Poly1305 (the `ring` feature
  of `purg-auth`; `ring` was already a dependency, for TLS). Sealing and
  opening one 8 KiB record takes 11 µs against 39 µs (720 against 210
  MB/s), and a 30-byte record 0.6 µs against 1.6 µs. The same 16 MiB read
  takes 115 to 140 ms encrypted (median 133) against 190 to 280 ms (median
  208) with the RustCrypto crate, with 68 to 83 ms in the clear: the
  cipher's share falls by about 2.2 times. The records are the same bytes
  (a test seals with each and opens with the other). The kernel, the
  probe and the guests keep the `no_std` crate, so the kernel's size does
  not change.
- The server's proof (below) costs nothing unless a client asks for it:
  the export signs on the first read at offset 64, one Ed25519 signature
  per conversation.

### Compatibility and downgrade

- A server always offers 64 bytes. A version 1 or 2 client reads only the
  first 32, the challenge, and its conversation stays in the clear, as
  before.
- A version 3 client refuses a server whose offer is only a challenge,
  with `no session key offered`. Otherwise someone in the middle could cut
  the offer to 32 bytes and keep the conversation in the clear.
- The server still takes version 1 and 2 proofs, so guests built before
  this keep working. `export -e` (`purg_abi::export::ENCRYPT`) refuses
  them with `encryption required`, and refuses an attach without a
  version 3 proof the same way, so a listener can insist on encrypted
  conversations (`styxlisten addr export -e /`). An export that trusts no
  key cannot encrypt, so with `-e` it refuses everyone. `rstyxd` always
  refused version 1 and 2 proofs; `rstyxd -e` also refuses to serve in
  the clear when it trusts nothing.
- `-A` and a server that answers `authentication not required` stay in the
  clear.

There is **no encrypt-only mode** (records without identities). Without a
key to sign with, an exchange cannot tell the real server from a man in
the middle, so it would stop only passive eavesdroppers while looking like
protection; a server that asks for no authentication lets anyone attach
anyway; and it would be one more mode to review. Inferno likewise pushes
`ssl` only with the secret its authentication agreed, and `-A` leaves the
conversation in the clear. A user who wants an encrypted conversation
gets a key (`getauthinfo`, or `mkauthinfo`) and authenticates.

### Server authentication

Once a version 3 proof is accepted, the server proves itself in turn. No
new version is needed: the flow reserved the read of the auth fid at
offset 64 for it, which travels in the records, and a client that does
not want it never asks.

```
C -> S  Tread afid 64 n                            (sealed)
S -> C  Rread  server proof                        (sealed; empty if none)
server proof = kind u8 || signature[64] || body
message      = "pgauth3 server" 0x00 || transcript[32] || bind[32]
                                     || kind u8 || body
transcript   = SHA-256(the client's signed message)   (the HKDF salt)
bind         = HKDF-Expand(prk, "pgauth3 server", 32)
```

`kind` and `body` are a public key (0) or a `pgcert1` line (1), as in
the client's proof, and the signature is by the body's key. `transcript`
fixes the challenge, both ephemeral keys, both names and the client's
identity; `bind` is a third output of the session's key derivation, so
only the two ends of this exchange can compute it. A proof is therefore
good for one session alone. A relay that holds its own end of a session
(it posed as the server, so it can seal records) cannot pass off a proof
the real server made for another session, even one it got as a client of
the real server: that proof signs another transcript and another bound
value. And because it arrives sealed under the session's keys, it also
confirms that the server holds them. The format is in
`purg_auth::server`.

The proof is made in the exporter's namespace. `export -k keyfile` (an
authinfo; `purgatorio.export_key`) signs with that key. Otherwise the key
is the guest factotum's, when one is mounted at `/mnt/factotum` and holds
an `ed25519` key (`role=server`, see "Factotum"), so the exporting prog
need not be able to read it; else the key the exporting user's own mount
would prove with: `#¤` (with its certificate),
`/usr/<user>/keyring/default`, `/usr/<user>/key`, then the public test
key. `rstyxd` uses its `-k keyfile`, or the same default sources. A
server with no key gives an empty read. The kernel's and the probe's own
exports sign with the test key or `#¤`.

**What the client trusts.** As in Inferno, where the server's certificate
is checked against the client's own signer list, the client checks the
proof against files in its own namespace, read at each mount:

- `/lib/keys/servers`: server keys held directly, each for the host its
  line names (a named key list: `<host> <key>`, or `*` for any host);
- `/lib/keys/serversigners`, or when there is no such file
  `/lib/keys/signers`: certificates from these signers are taken, for
  the host the certificate names (see "Signers for servers");
- `/lib/keys/revoked`: refused, key or signer, as for clients.

### Servers per host

Inferno does not check a server certificate's name, so there any holder
of a certificate from a trusted signer, a user included, can pose as any
server. Here a client that requires the server's proof (`-S`) and knows
the host it dialled requires that the proof is for that host:

- the **host** is the host field of the dial string as written:
  `fs.example` in `tcp!fs.example!styx` or `fs.example!styx`, `10.0.0.5`
  in `tcp!10.0.0.5!564`. It is not translated: a certificate names what
  clients type, and an address `/net/cs` returned would not match it.
  Names are compared with ASCII case ignored.
- a **key proof** passes when `/lib/keys/servers` lists the key for that
  host or for `*`;
- a **certificate** passes when its key is listed for that host or `*`, or
  when its name is the host and a trusted signer made it. A certificate
  from a trusted signer for any other name fails with `certificate name
  mismatch`.

So `-S` means "the server proved it is the host I dialled". A mount of a
fd or a `{command}` has no host; there `-S` takes any server this
namespace trusts, as before. `*` lines give the same any-host trust on
purpose: the test keys use it.

Where it is checked: the kernel's mount sees only a fd, never the address
it was dialled at, so the host is checked in the guest (`mount`, `import`,
`rcmd`, `cpu`, the SDK's `amount_host` and `check_server_host`). For a
mount the guest cannot verify the proof's signature, which needs the
session's keys, and the kernel cannot check the host. The two halves are
joined by reading the proof once:

1. after its proof, `mount -S` (through `fauth`; a dial string makes
   `mount -S` take that path even without `-k`) reads the auth fd at
   offset 64. The kernel fetches the server's proof from the server on
   that first read and keeps it; every later read, and the mount's own
   check, use those bytes.
2. the guest checks who the proof says the server is for the host
   (`purg_auth::server::check_identity`: the key or certificate, the
   signer, the expiry, revocation, the name);
3. the mount with `MSRVAUTH` makes the kernel verify the signature over
   the session and the trust in the same bytes
   (`purg_auth::server::check`, for any host).

A server cannot show the guest one proof and the kernel another. `rcmd`
and `cpu` hold the session's keys themselves and check everything at
once (`purg_auth::server::check_host`).

### Signers for servers

With one `/lib/keys/signers` list for both, a certificate made for a
user proves a host whose name is the user's: a user called `fs` (or
`10.0.0.5`) holds what `mount -S tcp!fs!styx` takes as the server's
proof. Issue #124 asked for a way to close that.

`/lib/keys/serversigners` is a key list of signers trusted for servers
only. When a client's namespace has it, it replaces
`/lib/keys/signers` for every server proof (the kernel's `MSRVAUTH`
check, `mount -S`, `import -S`, `rcmd -S`, `cpu -S`), and
`/lib/keys/signers` then counts for nothing but users. A site that wants
the two apart runs two signers: the users' in every server's
`signers`, the hosts' in every client's `serversigners`. A user
certificate then never proves a host, whatever its name, and a host
certificate never lets anyone log in to a server that does not list the
hosts' signer in its `signers`.

- Without the file nothing changes: `signers` serves both, as before.
  The stock root does not have it.
- A file that is there but empty, or holds only comments, trusts no
  signer for servers; one that cannot be read does the same. A line that
  does not parse is skipped, as in `signers`.
- `/lib/keys/servers` (keys held directly, per host) and
  `/lib/keys/revoked` apply either way.

The server side does not read it: what a server trusts for its clients
is still `/lib/keys/users`, `/usr/<user>/key.pub` and `/lib/keys/signers`.

**Requiring it.** Checking is the client's choice:

- `mount -S` and `import -S` (`purg_abi::mount::MSRVAUTH`, an addition:
  Inferno's mount(1) has no `-S`) read the proof before the attach and
  check it. The kernel does this for the plain mount and for one with an
  auth fd from `fauth`, so the SDK's `amount` gets it by passing the flag.
- `rcmd -S` and `cpu -S` check the proof `rstyxd` sends in its OK frame
  (`docs/rcmd.md`).

A server that sends no proof, asks for no proof itself, or a conversation
that is not version 3, fails with `server did not authenticate`. A
certificate for another host fails with `certificate name mismatch`. A
proof that does not verify, or a key or certificate the client does not
trust, fails with `server authentication failed`; the client's own files
say why. `-S` with `-A` is a usage error (`bad arg in system call` to
`mount`).

### Clients and the ABI

The kernel's own mount (`afd = -1`) does all of the above itself. Through
`fauth`, a read of 96 bytes (`SIGNED`) from the auth fd returns the offer
followed by `C`; the kernel made the key and keeps `c`. The guest (or its
factotum, which builds the message itself from those 96 bytes) signs and
writes the proof; the kernel recognises it as the proof for its key and
switches the conversation. A read of 32 bytes still gives the bare
challenge. The ABI goes from 0.6 to 0.7: a guest built for 0.7 reads 96
bytes and refuses a kernel that does not answer with them.

Version 0.8 adds `MSRVAUTH`, `export::ENCRYPT`, the read of the server's
proof and the errors `server did not authenticate`, `server
authentication failed`, `encryption required` and `conversation is
encrypted`. The wire format does not change: a 0.7 client never reads
offset 64, and a 0.8 client of a 0.7 server reads it as empty.

Version 0.9 changes what a guest sees, not the wire: an export without
`MNOAUTH` that trusts nothing refuses with `server trusts no keys`
instead of letting everyone in; a read of the auth fd at offset 64 or
later gives the server's proof as the kernel first read it, every time;
`mount` with `afd = -1` and `fauth` on a fd whose session is still alive
(an open auth fd, or a mount) fail with `device or object already in use`,
where they used to race that session's reader; and
`purgatorio.export_key` (`export -k`) is new.

## Post-quantum options

Everything above rests on X25519 and Ed25519. A large quantum computer
would break both: it could read a recorded conversation later by solving
the X25519 exchange ("harvest now, decrypt later"), and forge signatures.
The first threat is the urgent one, since recordings made today can be
read whenever such a machine exists. The options here are opt-in. The
defaults do not change, and old and new peers keep talking as before.

- **Key exchange:** a hybrid of X25519 and ML-KEM-768 (FIPS 203) in a new
  proof version, 4. `mount -C x25519mlkem768` uses it and requires it;
  `export -C x25519mlkem768` requires it of every client. Every export
  offers it to a client that asks.

ChaCha20-Poly1305 with a 256-bit key, SHA-256 and HKDF stay: Grover's
algorithm at best halves their security in bits, which leaves 128.

### Hybrid key exchange (version 4)

Version 4 is version 3 with an ML-KEM-768 exchange beside the X25519
one. The server makes an ephemeral ML-KEM key pair per auth fid, the
client encapsulates a secret to it, and both secrets go into the one key
derivation. The session is then safe while either X25519 or ML-KEM-768
holds: a flaw found in the young ML-KEM leaves X25519's protection, and
a quantum computer that solves X25519 still faces ML-KEM. This is the
construction of TLS's `X25519MLKEM768` group and of X-Wing.

```
C -> S  Tversion, Tauth afid uname aname              (clear)
S -> C  Rversion, Rauth
C -> S  Tread afid 0 1248
S -> C  Rread  offer_pq = challenge[32] || S[32] || ek[1184]
        (the client makes c, C = X25519(c, base), and encapsulates:
         (ct, k) = ML-KEM-768.Encaps(ek))
C -> S  Twrite afid 0  proof v4                       last clear request
S -> C  Rwrite  (or Rerror)                           last clear reply
        records from here on, each way
```

- **The long offer.** A read of the auth fid at offset 0 asking for at
  least 1248 bytes (`OFFER_PQ`) gives the 64-byte offer followed by `ek`,
  the server's ephemeral ML-KEM-768 encapsulation key. The server makes
  the key on the first such read and gives the same bytes to every later
  one, so it costs nothing for a client that never asks. A shorter read
  gives the version 3 offer, as before. A server built before version 4
  answers the long read with 64 bytes, which the client sees as "no
  post-quantum key". The server's proof stays at offset 64.
- **The proof.**

  ```
  proof v4 = 0x04 || kind u8 || signature[64] || C[32] || ct[1088] || body
  signed   = offer_pq[1248] || C[32] || ct[1088]           (2368 bytes)
  message  = "pgauth4" 0x00 || signed || len(uname) u16 LE || uname
                            || len(aname) u16 LE || aname || kind u8 || body
  ```

  `kind`, `body` and the check are as in version 3. The signature covers
  the whole long offer (so `ek`) and the ciphertext, under a context of
  its own.
- **Keys.** The client checks `ek` (FIPS 203's encapsulation key check)
  and refuses one that fails it.

  ```
  x      = X25519(s, C) = X25519(c, S)                   all zeros is refused
  k      = ML-KEM-768.Decaps(dk, ct) = the client's k
  prk    = HKDF-Extract(salt = SHA-256(message), ikm = k || x)
  c2s    = HKDF-Expand(prk, "pgauth4 c2s", 32)
  s2c    = HKDF-Expand(prk, "pgauth4 s2c", 32)
  bind   = HKDF-Expand(prk, "pgauth4 server", 32)
  ```

  The input key material is the ML-KEM secret first and the X25519 secret
  second, as in X-Wing and `X25519MLKEM768`. The salt is the hash of the
  signed message, which holds both X25519 keys, `ek` and `ct`, so the
  derivation binds every public value of both exchanges, the version and
  both identities. Where the records start, the record layer, the
  server's proof (with `bind` and `transcript` from this derivation) and
  the client's rules for the auth fd are as in version 3.

### Requiring it, and downgrade

A client or server either requires the hybrid exchange or does not use
it; there is no "use it if the peer offers it" mode, so there is no
quiet downgrade to reason about.

- `mount -C x25519mlkem768` and `import -C x25519mlkem768`
  (`purg_abi::mount::MPQ`) read the long offer. A server whose offer has
  no ML-KEM key, or that asks for no proof, fails the mount with `no
  post-quantum key offered`. A mount of an auth fd from `fauth` with
  `MPQ` fails with `post-quantum key exchange required` unless its proof
  was version 4. `-C x25519` names the default; any other name fails with
  `unknown algorithm`. Inferno's `mount -C` picks the algorithms pushed
  after authentication; here it picks the key exchange, since the record
  layer is fixed.
- `export -C x25519mlkem768` (`MPQ` on `sys->export`) refuses a version
  1, 2 or 3 proof, and an attach without a version 4 proof, with
  `post-quantum key exchange required`. It implies `-e`. Without it an
  export takes version 4 from any client that asks, and every older
  version as before.
- `-A` with `-C x25519mlkem768` is a usage error on both commands.

What an attacker on the wire can do against each:

- **Strip the ML-KEM key from the offer**, or rewrite the client's read
  to ask for 64 bytes: a client that requires the exchange fails with
  `no post-quantum key offered`. A client that does not require it was
  never going to use it.
- **Strip the ciphertext from a version 4 proof** and pass the rest on as
  a version 3 proof: the signature is over the `pgauth4` message, the
  long offer and the ciphertext, so the server refuses it with
  `authentication failed` (or `post-quantum key exchange required` when
  it requires version 4). Nothing can be removed from a version 4
  exchange without failing it.
- **Change `ek` or `ct`:** the signature fails, as for a changed X25519
  key in version 3. A changed `ct` alone would also give the server
  another ML-KEM secret (FIPS 203's implicit rejection), so the records
  could not be opened either way.
- **A client and server that both could, but neither requires:** they
  use version 3, as today. Turning the option on at either end is what
  protects a conversation; at the server end it protects every client.

The kernel enforces the auth fd rules of version 3 for version 4 too.
Through `fauth`, a read of at least 2368 bytes (`SIGNED_PQ`) at offset 0
fetches the long offer and returns `signed` above, with the kernel's own
X25519 key and ciphertext; the X25519 secret and the ML-KEM secret never
leave the kernel. A server that gives no ML-KEM key fails that read with
`no post-quantum key offered` and nothing is kept, so a guest may read
96 bytes for version 3 instead. The first read that makes a key decides
the exchange; every later read gives the same bytes, and a read too
short for them fails with `bad arg in system call`. The guest factotum
signs a version 4 message when the challenge it is given is 2368 bytes
long, as it signs version 3 for 96.

The ABI goes from 0.9 to 0.10: `MPQ`, the long offer through `fauth`,
version 4 proofs and the errors `no post-quantum key offered` and
`post-quantum key exchange required`. A 0.9 guest never asks for the long
offer, so it keeps talking version 3 to a 0.10 server.

Not covered yet: `rcmd`/`rstyxd` and Lethe have their own exchanges
(`docs/rcmd.md`, `docs/lethe.md`) and stay on X25519.

### Threat model

- **Harvest now, decrypt later:** a conversation on version 4 cannot be
  read by someone who records it and later breaks X25519, as long as
  ML-KEM-768 holds. This is what the option is for.
- **Authentication** is still Ed25519. A quantum attacker who can forge
  Ed25519 signatures in real time can pose as a user or a server; that is
  an active attack at the time of the conversation, not a later
  decryption. Post-quantum signatures are listed under "Decisions and
  open questions".
- **Forward secrecy** is as in version 3: both ML-KEM and X25519 keys are
  ephemeral, made per auth fid and dropped with the exchange.
- **Side channels.** ML-KEM is RustCrypto's `ml-kem`, whose decapsulation
  compares and selects in constant time; X25519 is `curve25519-dalek`'s.

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

## Users

Each prog acts as a user, as each Inferno process carries
`up->env->user` (`docs/abi.md`, "Users"). The user is what
`/dev/user` reads, the uname a mount authenticates and attaches as, the
user whose `/usr/<user>/keyring/default` and `/usr/<user>/key` a mount
proves with, the user an `export` signs its server proof as, the user
in `/prog/<pid>/status`, and the owner of files a prog creates in a RAM
tree (the probe's and the kernel's root).

The rules, which follow Inferno's:

- **The first prog is the host owner** (Inferno's `eve`): the
  platform's user, the login name of whoever started the emu, and
  `inferno` on the probe and the kernel. The host owner owns the
  devices.
- **A spawned prog inherits its parent's user**, at spawn. A later
  change in the parent does not reach children it already started.
- **Only the host owner can change user.** A prog acting as the host
  owner writes a name to `/dev/user` and acts as that user from then
  on. Any other prog gets `permission denied`, even writing its own
  name. Inferno's devcons has the same check (`iseve()`). So `runas`
  is one-way: a prog that became `bob` cannot become the host owner or
  anyone else again, nor can anything it starts.
- **Valid names** are single path elements of at most 64 bytes with no
  white space or control characters, so a name can always be
  `/usr/<user>`. Anything else is `bad arg in system call`.

What a user other than the host owner cannot do:

- **Use `#¤`.** `#¤` holds one key, the host owner's (the kernel's own
  mounts and exports sign with it). Its `ctl` and `rpc` are mode 0600,
  and only the host owner may open them. Before, `#¤` was mode 0666 and
  anyone could sign with its key; with one user per system that was the
  same thing. A guest factotum is an ordinary file server: who can use it
  is whoever can reach its mount, so a user's factotum belongs in that
  user's namespace.
- **Control another user's progs.** `/prog/<pid>/ctl` (kill, `killgrp`,
  limits, tracing) opens only for the prog's own user or the host owner.
  The other `/prog` files stay readable, as in Inferno.

What it does not change:

- **The namespace.** A prog's user does not decide what it can reach in
  its namespace; the namespace does, as in Inferno. `runas` forks the
  namespace and environment so the command's changes stay its own, but
  the command starts with everything the caller could reach. Running a
  command as a user with a smaller view is `runas` plus `bind`, or a
  namespace built with `nsbuild` or `pctl(NEWNS)`.
- **Host files.** The emu's root is the host file system, read and
  written with the host's permissions for the host user who runs the
  emu; their owners are the host's. Inferno's emu does the same.
- **The kernel's disk.** The kernel's purgfs device (`#K`) still acts as
  the host owner for every prog. `purg_fs::dev::DevFs::with_users` makes
  it check each prog's own user, but the kernel builds the device
  before the process table it would need (see "Decisions and open
  questions").
- **Servers.** What a server trusts is read in the exporter's namespace,
  whatever user the exporter acts as. An export run as `bob` proves
  itself with `bob`'s key, not the host owner's.

In Inferno too the host owner may become anyone. The host owner can
already read every user's key files on the system, so becoming a user
grants nothing new; it lets the host owner hand a command a user's
identity and authentication without handing it the host owner's.

`runas user cmd [arg ...]` is the command: it forks its namespace and
environment, takes a process group of its own (so a `killgrp` from the
command cannot reach the caller's progs), sets the user and runs `cmd`,
and exits with its status. A caller that is not the host owner gets
`runas: can't set user name: permission denied`.

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
| `auth/factotum [-m mntpt] [-s authinfo] [authinfo ...]` | [factotum(4)] A guest agent with Plan 9's `ctl` and `rpc` files (below). `-s` loads the key for server proofs. |
| `runas user cmd [arg ...]` | [runas(1)] Runs `cmd` as `user`, in a namespace, environment and process group of its own. Only the host owner may (see "Users"). |
| `auth/secstore [-s dir] [-C m,t] [-g file \| -G file \| -p file \| -r file]` | [secstore(1)] Files encrypted under a password, kept in a store directory (default `/usr/<user>/secstore`, which can be an authenticated import). `-g` gets a file into the current directory, `-G` prints it, `-p` puts one, `-r` removes one, and no option lists the store. |
| `crypt [-d] [-C m,t] (-k secret \| -f secretfile)` | [crypt(1)] Encrypts or decrypts standard input to standard output with ChaCha20-Poly1305 in 64 KiB chunks, under an Argon2id key. |
| `auth/aescbc -e\|-d [-k key] [-f keyfile]` | [secstore(1)] Encrypts or decrypts standard input in the file format of Plan 9's and Inferno's secstore, for files shared with them (below). Without `-k` or `-f` the key is read from `/dev/cons`. |
| `auth/ai2key [-t tag] keyfile ...` | [ai2key(8)] Prints each authinfo as the `key proto=ed25519 [tag] user=... !seed=... cert=...` line that loads it into `auth/factotum`'s `ctl`, so keys can be kept in a store and loaded later. |
| `netkey` | [netkey(1)] Reads a password, then challenges, and prints each response: the first 8 hex digits of HMAC-SHA256 keyed with SHA-256 of the password. It replaces the DES SecureNet calculator; it is only useful with a server that computes the same. |

### Merged, dropped or deferred

- **`logind`** is merged into `auth/signer`. In Inferno, `logind` issued
  certificates after a password check and `signer` issued them to keys an
  operator confirmed out of band, comparing a displayed "box" with
  `verify`. Here the password path is the only network path, and the
  operator path is `mkauthinfo`, run on the signer's machine. One network
  protocol is easier to review. Issue #38 asked again whether a separate
  `auth/logind` fits; it does not add anything. Its job (check a
  password held in keyfs, then sign the caller's key) is exactly what
  `auth/signer` does over `pglogin1`'s `cert` operation, and `keysrv`
  already takes the `passwd` operation on the same protocol. A second
  command would be a copy of `signer` under another name, or a second
  login protocol with its own review. Inferno's `logind` also spoke the
  station-to-station protocol over Inferno's keyring formats, which
  nothing here uses. A site that wants the Inferno name can start
  `auth/signer` under `listen` as its `logind` service.
- **`dsagen`, `rsagen`** are superseded. They printed DSA and RSA keys as
  factotum lines for Inferno's `infauth` and Plan 9's protocols. Every
  key here is Ed25519 and nothing reads DSA or RSA keys; the keys this
  system uses are made by `auth/createsignerkey` (a signer),
  `auth/mkauthinfo` and `getauthinfo` (a user's key and certificate).
- **`convpasswd`** is superseded. It turned Inferno's old
  `/keydb/password` file into keyfs's format. That file holds keys made
  from passwords with Inferno's own hashing; keyfs here keeps Argon2id
  verifiers, which cannot be made without the passwords, so there is
  nothing to convert. Users are added with `auth/changelogin`.
- **`ai2key`** is adapted. Inferno's turned an Inferno authinfo into a
  factotum `infauth` key; ours turns an authinfo here into factotum's
  `ed25519` key, the same line `auth/factotum keyfile` loads. There is
  no `server`, `dom` or `service` attribute: factotum picks `ed25519`
  keys by `user` and `role`.
- **`aescbc`** is kept for interchange. Its format, from Plan 9's
  `aescbc`, is `"AES CBC SHA1  2\n" || iv[16] || c0[16] || data ||
  mac[20]`: AES-128 in CBC mode under the first 16 bytes of
  SHA-1(`"aescbc file"` || password), `c0` a random block encrypted
  first, a final partial block XORed with the encryption of the last
  cipher block, and HMAC-SHA1 over `c0 || data` keyed with MD5 of the
  AES key. `-d` also reads the older format, with no header and a
  final block of 16 `X`s. The key is one SHA-1 of the password, so
  `crypt` and `auth/secstore` (ChaCha20-Poly1305 under Argon2id) stay
  the choice for files kept here; `aescbc` is for files that go to or
  come from a Plan 9 or Inferno secstore. Unlike Inferno's, it writes
  nothing until the whole input has been checked.
- **`verify`** keeps the name and checks a certificate, since there is no
  pending "box" to confirm.
- **`spki/verify`** is dropped. It checked SPKI S-expression certificates,
  which nothing here uses. Issue #38 asked whether a reader of
  Inferno-style SPKI certificates mapped onto Ed25519 is worth having.
  It is not, for now: Inferno's SPKI code verifies RSA and DSA
  signatures with MD5 and SHA-1 hashes, so no certificate an Inferno
  system made would verify with an Ed25519 key; a mapped format would be
  a third certificate syntax that only this system writes, with the
  same content as `pgcert1` (a name, a key, an expiry, signatures) and a
  far larger parser to review. SPKI's extra power, tags that grant
  named rights and chains of delegation, has no user here: trust is a
  signer list, and rights are file permissions in a namespace. If a
  real SPKI peer appears, the reader belongs in `purg-auth` beside
  `cert`, verifying SPKI's own algorithms.
- **`auth(2)`'s `server` side** is the kernel's export. Only the client side
  is in the SDK (`purgatorio::auth`).

### Factotum

`auth/factotum` mounts at `/mnt/factotum` (`-m` to change it) with Plan 9's
files. Keys are written to `ctl` as attribute lists. An attribute starting
with `!` is secret and is never read back. Values may be quoted with `'`.

```
key proto=ed25519 [user=<name>] [role=<role>] !seed=<hex> [cert='<pgcert1 line>']
key proto=pass server=<addr> user=<name> !password=<password>
delkey proto=<proto> [attr=value ...]
```

Reading `ctl` lists the keys, one per line, with secrets shown as
`!seed?`. Each authinfo named on the command line is loaded as an
`ed25519` key, and the one given with `-s` as an `ed25519` key with
`role=server`.

An `ed25519` key's `role` (`client`, `server` or `modsign`) says which
rpc role it is kept for; a key without one serves every role. A `start`
takes the first key kept for its role, and if there is none, the first
key with no role; either must match its other attributes. A `start`
that names no role is `role=client`. So a host's key loaded with `-s`
(or `role=server`) signs the export's server proofs even when a user's
key comes first, and is never used to prove a user. Before, `role=server`
took the first `ed25519` key, whatever it was for (issue #124).
`delkey ... role=<role>` removes only keys kept for that role.

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
- `ed25519` with `role=server`: `write <transcript[32] || bind[32]>` (the
  session's values, `purg_auth::server::SESSION` bytes), then `read`
  returns `ok <server proof>` (see "Server authentication"). Factotum
  builds the `pgauth3 server` message itself, so this role signs only
  server proofs. The kernel's export asks for it on the first read of
  its proof, and `rstyxd` through the SDK's `server_proof`: a server's
  key loaded into factotum (`auth/factotum keyfile`) never has to be
  readable by the exporting prog. Whoever can open factotum's `rpc` can
  make server proofs with it, as for client proofs. The key is the first
  one kept for `role=server`, else the first with no role (above).
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
  The server is authenticated only when the client asks (`-S`): without
  it, a client that dials a fake server talks to the fake server,
  encrypted. With it, the fake server needs a key or certificate the
  client trusts as a server's for the host dialled (see "Servers per
  host"), and cannot reuse the real server's proof from another session. What a man in the middle cannot do either way is
  relay the client's proof to the real server and keep the session,
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
  or `/usr`. The stock root trusts no key; a server that trusts none lets
  nobody in (see "Trust by default").
- **Password guessing.** Online guessing is limited only by connection
  rate; the signer does not lock accounts. The stand-in answer for an
  unknown user uses the default Argon2id cost, so an account made with
  another `-C` cost can be told apart from a missing one. The stored verifier is
  password-equivalent (see the scheme above); keyfs is the defence.
- **Secrets in memory.** Seeds and verifiers are not zeroised after use.

## Implementation and tests

- `crates/purg-auth` (`no_std` + `alloc`): `cert`, `keys`, `attr`,
  `proof` (the handshake and the server's check), `session` (the version 3
  X25519 exchange and key derivation), `record` (the record layer, with
  the `ring` cipher behind the `ring` feature), `server` (the server's
  proof) and,
  behind the default `login` feature, `login` (`pglogin1`, Argon2id,
  sealed files, `crypt`'s chunks). The kernel (`purg-core`) uses it
  without `login`. It builds for `aarch64-unknown-none` and
  `wasm32-wasip1`.
- Version 3 needs no new crate: X25519 is `curve25519-dalek` 4.1 (already
  under `ed25519-dalek`, BSD-3-Clause), and `chacha20poly1305` (now 0.11) and
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
- Server proof tests: `purg-auth`'s `server` tests (the key derivation
  against values computed separately with Python's `hmac`, a fixed proof,
  every changed byte, a proof for another session, a certified server,
  expiry and revocation) and `record`'s end of the sequence and the two
  ciphers agreeing; `styxcrypt`: a mount requiring the server accepts the
  real one, refuses an export with another key and accepts it once that
  key is in `/lib/keys/servers`, refuses a server that asks for no proof,
  checks the proof after `fauth`, refuses a fake server that seals but
  replays the real server's proof from another session (and accepts the
  same fake server with a fresh proof), `export` with `ENCRYPT`, a
  refused version 3 proof leaving the fd encrypted, and every read of the
  offer giving one key; `m3`'s `crypt_server`, `crypt_required` and
  `crypt_reuse`; `srvauth.sh` in the corpus (`mount -S`, `rcmd -S`, an
  impostor `rstyxd`, `export -e`, `rstyxd -e`); and the hosted
  `rcmd` test, which changes a byte of the proof on the way.
- Trust and per-host tests: `purg-auth`'s named key lists and
  `a_server_is_checked_for_its_host` (a certificate from a trusted signer
  for another host is refused, a key listed for the host passes, the
  identity check alone ignores the signature); `styxcrypt`'s
  `an_export_that_trusts_nothing_lets_nobody_in`,
  `the_server_proof_is_read_once` (a fake server that would answer the
  guest's read and the kernel's check differently) and
  `a_live_session_keeps_its_conversation`; `m3`'s `untrusted`,
  `server_proof_fd` and `in_use`; the corpus's `srvhost.sh` (`mount -S`
  and `import -S` refuse a server certified for another host, a key
  listed for the host passes and one listed for another does not, and
  `rcmd` to such an impostor gets no `/n/client`: refused under `-S`, and
  without `-S` its own mount of `/n/client` fails) and `srvauth.sh`
  (nothing trusted: an export and `rstyxd` let nobody in).
- Post-quantum tests: `kem`'s known answers (a key pair, an
  encapsulation and an implicit rejection, computed separately with the
  pure-Python `kyber-py`), a malformed encapsulation key refused, and
  round trips; `session`'s version 4 derivation against Python's `hmac`,
  both sides agreeing, a changed ciphertext, and malformed long offers;
  `proof`'s `version_4` (every part of the long offer and the ciphertext
  bound, a proof stripped to version 3 refused, a version 3 proof still
  taken by a server that made the long offer); `styxcrypt`'s
  `a_pq_mount_is_encrypted`, `fauth_with_a_version_4_proof`,
  `an_export_can_require_pq`, `a_stripped_pq_offer_is_refused`,
  `a_shortened_offer_request_is_refused` and
  `a_proof_stripped_to_version_3_is_refused`; and the corpus's
  `pqmount.sh` (`mount -C`, `import -C`, `export -C`, a classical client
  of a plain export and of a post-quantum-only one, a server that asks
  for no proof), in the emu, the probe and the kernel.
- Fuzzing: the `record` fuzz target also feeds its input to the server
  proof's check, and its smoke test (`cargo test -p purg-auth --test
  fuzz_smoke`) adds genuine server proofs, with keys and certificates,
  changed or cut short. A real run needs cargo-fuzz and a nightly
  toolchain (`cargo +nightly fuzz run record` in `fuzz/`), which the
  build hosts do not have.
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
- `auth/aescbc`: unit tests against a vector made separately (Python's
  `hashlib` and `hmac`, and `openssl` for the AES blocks) in both formats,
  every length up to five blocks, and every changed byte; `aescbc.sh` in
  the corpus runs it and `auth/ai2key` everywhere the corpus runs.

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
  from a signer the client trusts): done, at the client's request (issue
  #105; see "Server authentication").
- **Trust by default** (issue #91): the stock root trusts no key, and a
  server without `-A` that trusts nothing lets nobody in (see "Trust by
  default").
- **Servers per host** (issue #124): under `-S` a server must prove the
  host dialled (see "Servers per host").

Proposed in issue #38 and #124, for the owner's review:

- **Per-prog users** (see "Users"): a user per prog, inherited at spawn,
  changed only by the host owner through `/dev/user`; `#¤` and other
  users' `/prog/<pid>/ctl` closed to everyone but the host owner and
  the prog's user.
- **Signers for servers** (see "Signers for servers"):
  `/lib/keys/serversigners`, when present, replaces `/lib/keys/signers`
  for server proofs.
- **Factotum's server key** (see "Factotum"): a `role` attribute on keys,
  and `auth/factotum -s`.

Still open:

1. **OPAQUE.** Move to an augmented PAKE once the RustCrypto crates for it
   are stable, so a stolen keyfs entry is not password-equivalent?
2. **Servers that act for several users.** Progs now carry their own
   user, but an export serves every client as the exporting prog's user;
   it does not take on the user a client proved. Inferno's exportfs does
   not either.
3. **Users on the kernel's disk.** `#K` is built before the process table
   exists, so it acts as the host owner for everyone (see "Users").
4. **`/chan` files** (`#s`) are owned by the host owner, whoever posts
   them; Inferno makes them the poster's.
5. **Post-quantum signatures.** Certificates, proofs and module
   signatures are Ed25519 only. ML-DSA-65 (FIPS 204) beside Ed25519 would
   protect authentication against a quantum attacker in real time; see
   "Post-quantum options".
6. **Post-quantum `rcmd` and Lethe.** Their exchanges are X25519 only.
7. **A post-quantum default.** Should a later release make
   `x25519mlkem768` the default for `mount` and `export`, once both ends
   in the field speak version 4?
