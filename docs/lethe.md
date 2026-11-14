# Lethe: stateless file access

This is the design for Lethe (issue #99), a protocol for reading static
file trees that keeps no state on the server. Styx keeps fids, open files
and tags per connection; Lethe keeps none. Any request can be answered by
any replica or a cache in between, a lost connection loses nothing, and a
server can be very small.

The owner decided, and this design follows:

- The server is stateless.
- Authentication and encryption are the default. A clear mode, with
  neither, exists as an explicit option on both ends (`lethed -A`,
  `lethe -A`), in the way `-A` works for Styx mounts. Neither end falls
  back to it by itself. Section 17 says what it gives up. (The first
  decision was "required, no clear mode"; the owner revised it in issue
  #99.)
- The first version reads only. Writes come later, without a new wire
  version.
- Streaming comes later, as an open mode of Lethe that is a property of
  the request. It has its own section below.

Section 14 says what the first cut implements. In short: the codec, the
ticket handshake, `stat`, `read` and `readdir` over TCP, the host server
`lethed`, the kernel device `#λ` and the `lethe` command.

## 1. Why stateless helps for static trees

Boot roots, `/bin`, `/lib` and shared module stores are read by many
machines and almost never written. For them:

- **Replicas and caches are free.** A request carries everything needed to
  answer it, so a load balancer can send each one anywhere, and a cache
  can answer from its copy after checking the version.
- **Failures cost nothing.** A Styx client whose connection drops loses
  its fids and every open file; its mounts are dead. A Lethe client dials
  again and resends the request. A restarted server answers as if nothing
  happened.
- **The server is small.** No fid table, no tags to flush, no per-client
  memory to bound. A server is a function from a request to a reply.
- **Content can be checked end to end.** A module has a SHA-256 digest
  (`docs/signing.md`) and signatures inside it, so its integrity does not
  depend on the channel or on a cache being honest.

The price: no open files, no locks, no server-side position, no
notification of change. A client learns of a change only when it asks.
For static trees that is the right trade.

## 2. Model

A Lethe server serves one tree. Names are absolute paths in the tree:
`/`, `/lib/ndb/local`. A client asks for one of three things about a path:

- `stat`: the entry (a Styx `Dir`).
- `read`: a byte range of a file.
- `readdir`: a page of a directory's entries, resumed with a cookie.

Every reply carries the file's qid. The qid's version changes when the
file does, which is what clients cache by.

Before any of this the client authenticates once and gets a **ticket**: its
identity, its rights and a session key, sealed under a key only the
servers hold. Each request carries the ticket and is sealed with the
session key. The server opens the ticket, checks the request and answers.
It remembers nothing afterwards.

In the clear mode (Section 17) there is no ticket: a request is the plain
operation, the reply the plain answer, and the server answers as the user
`none`.

## 3. Framing

Every message, in both directions, is one frame:

```
size[4] version[1] type[1] id[4] body
```

- `size` (u32 LE) counts the whole frame, these four bytes included, as a
  Styx message's `size` does.
- `version` is 1 for this protocol (`lethe1`). A server answers another
  version with `Rerror` "unknown lethe version" and the client gives up.
- `type` says what the body is (Section 4).
- `id` (u32 LE) is chosen by the client and copied into the reply. It only
  matches replies to requests on one connection; it is not state.
- A frame is at most 65536 bytes. A larger `size` is refused before the
  body is read.

All integers are little-endian and decoded with `from_le_bytes`. A string
`s[2]` is a u16 LE length and that many bytes of UTF-8. Decoding is
strict: a frame must end exactly where its last field does, so each
message has one encoding.

Over TCP, frames follow each other on the stream. A client may send a new
request before the last reply arrives (replies may come back in any order,
matched by `id`). `#λ` sends one request at a time per connection and
opens more connections for concurrency, except for readahead, which sends
a few reads on one connection before reading their replies (Section 14).

## 4. Messages

### Outer frames

| type | name | direction | body | sealed |
|---:|---|---|---|---|
| 1 | `Tauth` | C -> S | see Section 6 | no (signed) |
| 2 | `Rauth` | S -> C | `eph[32] nonce[24] sealed(grant)` | under `s2c` |
| 3 | `Treq` | C -> S | `ticket s[2] nonce[24] sealed(request)` | under `c2s` |
| 4 | `Rreq` | S -> C | `nonce[24] sealed(reply)` | under `s2c` |
| 5 | `Rerror` | S -> C | `ename s[2]` | no |
| 6-8 | reserved | | streams (Section 11) | |
| 9 | `Tclear` | C -> S | `request`: `op u8 fields` | no |
| 10 | `Rclear` | S -> C | `reply`: `op u8 fields` | no |
| 11-15 | reserved | | later outer frames | |

`Rerror` in the clear is only for failures before the server can seal: a
bad frame, a ticket that does not open or has expired, a request that does
not open or is outside its time window, and every `Tauth` failure. Errors
about files (`file does not exist`, `permission denied`) are sealed inside
`Rreq`, so an eavesdropper does not learn which names exist.

`Tclear` and `Rclear` are the clear mode's request and reply (Section 17).
They use two of the outer types the design kept in reserve, inside
version 1: no new version, and no flag in the header. Their bodies are the
inner request and reply of the table below, as they are: a `Tclear` is the
header, the op and its fields (path and arguments), with no ticket, no
nonce, no `time` and no seal; an `Rclear` is the header and the reply. For
example a `stat` of `/` with id 5 is these 14 bytes:

```
0e 00 00 00  01  09  05 00 00 00  10  01 00 2f
size         v   Tclear  id       Tstat  path "/"
```

Each is one self-contained frame, as the sealed forms are, so it maps to
one datagram the same way (Section 12). A file error (`file does not
exist`) comes back inside the `Rclear` as the inner `Rerror`; the outer
`Rerror` is for a frame the server refuses, as before. A server not
started with `-A` refuses every `Tclear` with the outer `Rerror`
`authentication required`, before looking at its body. A server from
before the clear mode answers it `bad lethe message`.

### Inner requests and replies

The plaintext of a `Treq` is `time u64 op u8 fields`; of an `Rreq`, `op u8
fields`. `time` is the client's estimate of the server's clock (Section 8).
The body of a `Tclear` is `op u8 fields` with no `time` (there is no replay
window to check it against), and the body of an `Rclear` is `op u8 fields`.

| op | name | fields | first cut |
|---:|---|---|---|
| 0x10 | `Tstat` | `path s[2]` | yes |
| 0x11 | `Rstat` | `stat[n]`, a Styx stat entry (it starts with its own `size[2]`) | yes |
| 0x12 | `Tread` | `path s[2] offset u64 count u32 cond u8 vers u32` | yes |
| 0x13 | `Rread` | `qid[13] unchanged u8 data[count u32]` | yes |
| 0x14 | `Treaddir` | `path s[2] cookie s[2] count u32` | yes |
| 0x15 | `Rreaddir` | `qid[13] cookie s[2] entries[count u32]` | yes |
| 0x16 | `Tdigest` | `path s[2]` | reserved |
| 0x17 | `Rdigest` | `qid[13] digest[32]` (SHA-256 of the content) | reserved |
| 0x18 | `Twalk` | `path s[2] nname u16 name s[2]*nname` | yes (added) |
| 0x19 | `Rwalk` | `nqid u16 qid[13]*nqid ename s[2] stat[n]` | yes (added) |
| 0x20-0x2f | writes | Section 10 | reserved |
| 0x40-0x4f | streams | Section 11 | reserved |
| 0x7f | `Rerror` | `ename s[2]` | yes |

- **`Tread`** asks for at most `count` bytes (at most 32768) at `offset`.
  With `cond` 1 the client already holds data for version `vers`: if the
  file is still at that version the server answers `unchanged` 1 with no
  data, as HTTP's `If-None-Match` gets 304. With `cond` 0, `vers` is 0 and
  ignored. `Rread`'s qid is the file's at the time of the read; a short
  read means end of file.
- **`Treaddir`** asks for at most `count` bytes of packed Styx stat
  entries, starting after `cookie` (empty for the first page). The reply's
  `cookie` resumes the listing; empty means there is no more. The cookie
  is opaque to the client. `lethed` uses the last name it returned and
  lists names in byte order, so a page resumes at the right place even if
  entries were added or removed in between. The reply's qid lets a client
  notice that the directory changed while it was paging.
- **`Twalk`** walks `names` (at most 64) one after another from the
  directory `path`, as a 9P `Twalk` does from a fid, and answers what a
  `Tstat` of each joined path would have, in one round trip: the qid of
  every name walked, then either an empty `ename` and the stat entry of
  the last name (of `path` itself when there are no names), or the
  reason the next name could not be walked (`file does not exist`, `not
  a directory`, `permission denied`, `file name syntax`) and no entry.
  So `nqid` is the number of names walked, and less than `nname` exactly
  when `ename` is not empty. `path` itself is checked as a `Tstat` of it
  is (rights, prefix, search on every directory above it), and a failure
  there is the plain `Rerror`. Each name then needs the one before it to
  be a directory the requester can search. A walk of a 17-element name
  costs one round trip where a `Tstat` per element cost 17.

  `Twalk` came after the first cut, at a code version 1 left unassigned,
  so a server from before it answers `not implemented` (the rule for
  unknown ops below). A client that gets that goes back to a `Tstat` per
  element and does not ask that server again. The version byte does not
  change.
- **Paths** start with `/`, have no empty, `.` or `..` element, no NUL,
  and no trailing `/` except the root. Elements are at most 255 bytes and
  paths at most 1024. Anything else is `file name syntax`. The client
  resolves `..` itself, as Inferno's names are lexical.
- **Unknown ops** get `not implemented`. Write ops get `permission denied`
  from a read-only tree. A decoder treats reserved codes as known-but-
  unsupported, never as garbage, so later clients get a clear error from
  older servers.

## 5. Versions and caching

The qid is Styx's: `type[1] vers[4] path[8]`. `path` identifies the file
while it exists; `vers` changes whenever its content or size does.
`lethed` takes both from the host file as `#U` does: `path` from the
device and inode, `vers` from the modification time (with nanoseconds) and
the size. A file rewritten in place within one timestamp tick, to the same
size, keeps its version; hosts with coarse timestamps should not be used
for trees that change while served.

The client caches file data by `(path, vers)` and directory listings by
`(path, vers)`:

- A read of data the client holds sends `cond` 1 with its version and
  gets either `unchanged` (no data on the wire) or the new data and qid,
  which replace the cache.
- **The attribute cache.** What the client learnt about a path (a stat
  entry, a walk's qid, a listing's entries, a read's qid) is trusted for
  a short lifetime, as NFS trusts attributes for `acregmin`: 1 s by
  default, set per attach (`lethe -t ms`, the `cache` line of `#λ`'s
  `clone`). Within it a walk or a stat costs no round trip, and cached
  blocks of the version it confirmed are served without asking, so a
  file is revalidated once per lifetime, not once per block. A file
  changed on the server is therefore seen up to one lifetime late.
  NFS's default is 3 s; 1 s keeps the window short for trees that are
  edited while served, and is long enough for the work it is for (a
  build or an `ls -l` sees each name many times within a second).
- With a lifetime of 0 nothing is trusted: every stat asks, and every
  read asks once (a read that spans several cached blocks asks for the
  first and serves the rest of that version). Cached data is then never
  served for a version the server no longer has. Two things fetched for
  the operation in progress are used once however old: the qids a
  `Twalk` returned for the names of the walk that sent it, and blocks
  read ahead for a sequential reader.

**Content addressing** (later): `Tdigest` returns the SHA-256 of a file's
content, so a client can fetch a module and check it against a digest it
already trusts, or find it in a cache keyed by digest. Signed modules carry
their own signatures and are checked by the loader in any case.

## 6. Authentication and tickets

Statelessness makes authentication the main problem: the server cannot
keep a session key for a connection the way a Styx mount does after the
version 3 handshake (`docs/auth.md`). Issue #99 lists two options:

1. **Self-contained tickets.** An authenticated exchange gives the client
   a ticket: its identity, a session key and an expiry, sealed under a key
   only the servers hold. Every request carries the ticket and is sealed
   with the session key. This is the Kerberos ticket and the TLS 1.3
   session ticket.
2. **An encrypted transport per connection**, with the request layer above
   it still stateless.

**Lethe uses tickets.** The second option brings back exactly the state
Lethe is meant to remove: a connection's keys live on one server, a
restart or a move to a replica means a new handshake, and UDP needs a
separate answer. With tickets:

- Any server holding the ticket key can answer any request. Replicas share
  the key file and nothing else.
- A restarted server with the same key file accepts every ticket it issued
  before; with a fresh key (the default) clients get "unknown ticket key"
  and authenticate again, transparently.
- Each request is a sealed datagram already, so a UDP mapping needs no new
  cryptography.

The cost is per request: open the ticket (one AEAD), derive the session
keys (two HKDF expansions), open the request and seal the reply. That is
microseconds against the round trip.

### The exchange

One round trip, started by the client, with no server challenge (a
challenge would be state the server has to remember until the reply):

```
C -> S  Tauth  = eph_c[32] rights u8 prefix s[2] uname s[2]
                 kind u8 body s[2] signature[64]
S -> C  Rauth  = eph_s[32] nonce[24] sealed_s2c(grant)
grant   = now u64 expires u64 rights u8 prefix s[2] ticket s[2]
```

- `eph_c` is a fresh X25519 key. `kind` and `body` are as in a mount
  proof: a public key (0) or a `pgcert1` line (1).
- The signature is by the body's key over

  ```
  "lethe1 auth" 0x00 || eph_c || rights || prefix s[2] || uname s[2]
                     || kind || body s[2]
  ```

  a context string of its own, so it can never verify as a mount proof, a
  certificate or a module signature.
- The server checks it with `purg_auth::proof::check_cert` and the same
  trust files as an export (Section 9): a directly held key, or a valid
  certificate for `uname` from a trusted signer, neither revoked.
- It makes `eph_s`, computes `shared = X25519(eph_s, eph_c)` (all zeros is
  refused), and derives

  ```
  transcript = signed message || eph_s
  prk        = HKDF-Extract(salt = SHA-256(transcript), ikm = shared)
  master     = HKDF-Expand(prk, "lethe1 master", 32)
  c2s        = HKDF-Expand(master, "lethe1 c2s", 32)
  s2c        = HKDF-Expand(master, "lethe1 s2c", 32)
  ```

- `grant` is sealed under `s2c` with a random 24-byte nonce and the frame
  header and `eph_s` as associated data. `now` is the server's clock,
  `expires` the ticket's end, both in seconds since 1970 UTC. `rights` and
  `prefix` are what was granted (Section 9).

A recorded `Tauth` replayed later gets a fresh `eph_s` and a grant sealed
under keys only the holder of the original `eph_c` secret can derive, so a
replay is useless; it costs the server one signature check and one X25519.
Both ephemeral secrets are dropped after the exchange, so someone who
later steals the user's key cannot read recorded traffic. The ticket key
is different: it opens recorded tickets, which hold `master`, so a stolen
ticket key exposes the traffic of every ticket sealed under it. Rotating
the ticket key ("Ticket keys" below) bounds that window. This is the same
trade TLS session tickets make.

As with mount version 3, the server is not authenticated: a client that
dials a fake server talks to the fake server, encrypted. A man in the
middle cannot relay the client's `Tauth` to the real server and read the
result, because the grant is sealed under keys that need `eph_c`'s secret.
A server proof can be added to `Rauth` later without a new version: the
grant would gain a trailing `kind u8 signature[64] body s[2]` over
`"lethe1 server" 0x00 || SHA-256(transcript)`. A client asks for it with
bit 7 of `rights`, which is reserved for this, so an old client, whose
strict decoder would refuse the longer grant, never receives it.

### The post-quantum exchange

`lethe -C x25519mlkem768` asks for a hybrid X25519 + ML-KEM-768 exchange,
as `mount -C x25519mlkem768` does (`docs/auth.md`, "Hybrid key exchange
(version 4)"), so a recorded `Tauth` and its traffic stay closed to
someone who later breaks X25519, as long as ML-KEM-768 holds. Lethe has
no server challenge, so the client makes the ephemeral ML-KEM key and the
server encapsulates to it, in the same one round trip:

```
C -> S  Tauth2 = eph_c[32] ek[1184] rights u8 prefix s[2] uname s[2]
                 kind u8 body s[2] signature[64]
S -> C  Rauth2 = eph_s[32] ct[1088] nonce[24] sealed_s2c(grant)
signed     = "lethe2 auth" 0x00 || eph_c || ek || rights || prefix s[2]
                            || uname s[2] || kind || body s[2]
(ct, k)    = ML-KEM-768.Encaps(ek)                 the server
x          = X25519(eph_s, eph_c)                  all zeros is refused
transcript = signed || eph_s || ct
prk        = HKDF-Extract(salt = SHA-256(transcript), ikm = k || x)
master     = HKDF-Expand(prk, "lethe2 master", 32)
```

`c2s`, `s2c`, the grant and the ticket are as for `Tauth`; the grant's
associated data is the header, `eph_s` and `ct`. `Tauth2` and `Rauth2`
are frame types 11 and 12, from the reserved range. The ticket's rights
byte gets bit 6 (`rights::PQ`), which no client can ask for, so a server
can tell a ticket from the hybrid exchange from one that is not.

There is no "use it if the other end can" mode:

- A client that asked takes only an `Rauth2`. An `Rauth` fails with `no
  post-quantum key offered`, and so does `bad lethe message`, which is
  what a server built before this answers a frame type it does not know.
- `lethed -C x25519mlkem768` refuses a `Tauth`, and a request on a ticket
  without `rights::PQ`, with `post-quantum key exchange required`.
  Without it a server takes either exchange.
- Turning a `Tauth2` into a `Tauth` (dropping `ek`) does not verify: the
  signature is over the `lethe2 auth` message. A changed `ek` or `ct`
  fails the signature or the grant's seal.

The kernel's `#λ` does the client's side (`kx x25519mlkem768` in the
`clone` write). The cost is one ML-KEM key generation and decapsulation
for the client and one encapsulation for the server per ticket, about 0.1
ms natively, and 2.3 KB more on the wire per ticket; requests do not
change.

### The ticket

The ticket is opaque to the client:

```
ticket = keyid u32 || nonce[24] || XChaCha20-Poly1305(ticket key, aad, plain)
aad    = "lethe1 ticket" 0x00 || keyid
plain  = master[32] issued u64 expires u64 rights u8 uname s[2] prefix s[2]
keyid  = first 4 bytes of SHA-256("lethe1 ticket key" 0x00 || ticket key), as u32 LE
```

The nonce is random. XChaCha20's 24-byte nonce makes random nonces safe
for any number of tickets under one key, which plain ChaCha20-Poly1305's
12 bytes would not.

### Ticket keys

A ticket key is a 32-byte file (the seed file format of `/lib/keys`),
named with `lethed -t`. Without `-t`, `lethed` makes a random key at start.
The `keyid` lets a server tell "not my key" (`unknown ticket key`: the
client authenticates again) from "tampered" (`bad ticket`). Rotation:
install a new key file, keep the old one as a second `-t` for one ticket
lifetime, then remove it; tickets under the old key keep working until
they expire. The first cut takes one key.

### Requests

```
Treq  = ticket s[2] nonce[24] XChaCha20-Poly1305(c2s, aad = header || ticket, request)
Rreq  = nonce[24]  XChaCha20-Poly1305(s2c, aad = header || request nonce, reply)
```

`header` is the frame's first 10 bytes, so the size, type and id are
covered. The reply's associated data includes the request's nonce, so a
reply cannot be moved onto another request. Both nonces are random: the
server never reuses one even when it answers a replayed request with
different data, which a counter derived from the request would risk.

The server opens the ticket (`bad ticket` / `unknown ticket key` /
`ticket expired`), derives the keys, opens the request (`bad record mac`),
checks its time (`request expired`), checks rights and prefix, and only
then looks at the tree.

## 7. Errors

Errors are Inferno strings, as Styx carries them. File errors come from
the tree: `file does not exist`, `permission denied`, `not a directory`,
`file is a directory`, `file name syntax`. Lethe adds:

| error | where | client action |
|---|---|---|
| `unknown lethe version` | clear | give up |
| `bad lethe message` | clear or sealed | give up (a bug) |
| `bad ticket` | clear | authenticate again |
| `unknown ticket key` | clear | authenticate again |
| `ticket expired` | clear | authenticate again |
| `request expired` | clear | resynchronise the clock (authenticate again) and resend |
| `bad record mac` | clear | give up on this request (tampering) |
| `not implemented` | sealed | give up |
| `authentication required` | clear | give up: a clear request (`lethe -A`) reached a server not started with `-A` |
| `authentication not required` | clear | give up: a `Tauth` reached a server started with `-A` that trusts no key for the user |

The last two are the mode mismatches, with the strings Styx uses for the
same two cases (`docs/auth.md`). Neither makes the client change mode: the
attach fails with the error, and the user decides. In the clear mode
"sealed" in this table means inside the `Rclear`.

`Tauth` failures use `docs/auth.md`'s strings: `authentication failed`,
`certificate expired`, `certificate revoked`, `untrusted signer`,
`certificate name mismatch`.

## 8. Replay

The server keeps no record of requests it has seen, so it cannot refuse a
replay by memory. Instead:

- **A time window.** Each sealed request carries `time`, the client's
  estimate of the server's clock. The client needs no synchronised clock:
  `Rauth` gives the server's `now`, and the client keeps the difference
  from its own clock. The server refuses a request whose time is more than
  `W` seconds (default 60, `lethed -w`) from its own with `request
  expired`. A recorded request is useless after `W`.
- **Ticket expiry.** A ticket lasts `L` seconds (default 3600, `lethed -e`).
  After that nothing sealed under it is accepted. The client
  authenticates again before expiry, or on `ticket expired`.
- **Idempotence.** Every first-cut operation is a read. A request replayed
  inside the window makes the server do the same read again and seal the
  answer under a fresh nonce, for keys the attacker does not hold. It
  learns the reply's length and nothing else.

Writes (Section 10) are not idempotent in general, so they get stronger
rules: they are conditional on a version, which makes a replayed write
fail once the first has changed the version, and a server may keep a
replay cache of request nonces for the last `W` seconds. That cache is
soft state: losing it in a restart only reopens the window, and servers
that share tickets but not caches accept a replay at most once each.

## 9. Authorization

- **Who may get a ticket** is decided as for an export (`docs/auth.md`,
  "Where the server looks"): the server's directly held keys
  (`usr/<uname>/key.pub` style public key files), trusted signers
  (`/lib/keys/signers`), and revocations (`/lib/keys/revoked`), in the same
  formats. `lethed` reads them from host paths given on its command line,
  at every `Tauth`, so an edit takes effect on the next ticket. A damaged
  revocation list fails closed. A server with no key and no signer refuses
  every client with `server trusts no keys`, as an export that trusts
  nothing does (`docs/auth.md`, "Trust by default"). The only way to
  serve without authentication is to ask for it with `-A` (Section 17).
- **Clear requests** are answered only by a server started with `-A`, as
  the user `none`, Inferno's unauthenticated identity. `none` owns
  nothing: it gets the other bits of every file and directory, even in a
  tree whose owner is called `none` (`lethed -u none`). It has the read
  right over the whole tree, with no narrower prefix, and the same search
  check on each directory above the path.
- **What a ticket may do.** The identity in the ticket is checked against
  the tree's permissions at every request, as Inferno checks a user against
  a file's mode: the owner's bits when the ticket's user is the file's
  owner, else the other bits (`lethed` has no group database, so group
  bits are not used). `read` needs `r` on the file, `readdir` `r` on the
  directory, and every path needs `x` on each directory above it.
- **Narrower tickets.** A ticket carries **rights** (bit 0 read, bit 1
  write, bit 2 stream, the rest reserved) and a **prefix**, a subtree it is
  limited to. The client asks for both in `Tauth`; the server grants the
  intersection of what was asked and what it allows (`lethed` allows read
  only) and says what it granted. A request outside the prefix, or needing
  a right the ticket lacks, is `permission denied`. A ticket can be handed
  to a less trusted program to read one subtree and nothing else. It never
  widens what the identity may do.
- **Revocation** takes effect for new tickets at once and for issued
  tickets when they expire, so `L` is also the revocation latency. A
  deployment that needs faster revocation shortens `L`; renewal is one
  round trip.

## 10. Writes (later)

The first cut has no writes. The wire leaves room for them without a new
version: write ops are reserved (0x20-0x2f), the rights byte has a write
bit, and a read-only server answers them `permission denied`.

The options, and what each costs a stateless server:

1. **Conditional, idempotent writes.** `Twrite path offset if-vers data`
   applies only if the file is still at version `if-vers`, as HTTP
   `If-Match` does; `Rwrite` returns the new qid. A retry after a lost
   reply finds the version changed and gets `version conflict` together
   with the current qid, which tells the client whether its own write
   landed (the new version follows from it) or someone else's did.
   `Tcreate`, `Tremove` and `Trename` are conditional the same way (on the
   parent directory's version, or "must not exist"). No server state; two
   writers to one file serialise through the version; caches stay correct
   because every read checks versions anyway. The cost is a conflict for
   the loser, which retries or gives up. **This is the recommended first
   step.**
2. **Leases.** Time-bounded read or write leases (NFSv4 delegations, SMB
   oplocks, Gray and Cheriton) let a client cache and write without
   checking each time. The lease table is state, but soft: it expires by
   itself, so a restarted server waits out the longest lease before
   granting conflicting ones, and needs no recovery protocol. Replicas
   would need to share the table, which is what Lethe avoids; leases suit
   one server with many caching clients.
3. **Whole-file and append-only updates.** For module stores and boot
   trees: put content by digest (`Tput digest data`, idempotent by
   construction), then swap a name to it atomically and conditionally
   (`Tlink path digest if-vers`), as Venti and Fossil do. Readers never see
   a half-written file and replicas can copy content lazily by digest.

| | server state | restart | several writers | caches |
|---|---|---|---|---|
| conditional | none (optionally a replay cache) | nothing lost | loser gets a conflict | correct: reads check versions |
| leases | lease table (soft) | wait out leases | serialised by leases | coherent while leased |
| whole-file | none | nothing lost | last swap wins, conditionally | content is immutable |

Write tickets need the write right, which `lethed` would grant only to
identities configured for it.

## 11. Streaming (later)

For audio, video, sensor and telemetry streams, where late data is
worthless and a lost packet is better skipped than resent. Nothing here is
built in the first cut; the base protocol only keeps room for it (outer
types 6-8, ops 0x40-0x4f, the stream right).

### Decided by the owner

- **An open mode of Lethe, not a separate protocol.** The same names,
  tickets and permissions apply, and a stream file binds and unions like
  any other.
- **A property of the request.** A stream request names the file, a start
  position and its options. Data then flows until the requester says
  stop. There is no open and no per-file mode.
- **Both directions.** A client streams from a file (the server sends) or
  to a file (the client sends): a camera feeding a server file, telemetry
  uploads, a guest sending audio to the host. The same loss rules apply
  either way.
- **Auth and encryption are still required.** The clear mode (Section 17)
  covers the request operations only; no clear stream frame is defined.
- **Multicast and broadcast: maybe.**

### Sketch

```
Tstream  op 0x40  path s[2] start u64 dir u8 rate u32 options s[2]
Rframe   type 6   (outer)  stream id u32, seq u64, gap u32, sealed frame
Tframe   type 7   (outer)  the same, client to server
Tstop    op 0x42  stream id u32       idempotent
Rreport  type 8   receiver's loss and rate report
```

A stream is named by the `id` of the `Tstream` that started it. Every
frame is sealed on its own with the ticket's session key (XChaCha20 with
the stream id and sequence number bound in the associated data), so frames
decrypt independently and a receiver keeps a replay window rather than
demanding order.

### Conflicts with the stateless core

| core rule | what streaming needs | decision and tradeoff |
|---|---|---|
| The server keeps no state | a running stream: who, where, which position | **Soft state, like a lease.** A stream ends on `Tstop`, at ticket expiry, or when the receiver's reports stop for a set time (a few seconds). A lost `Tstop` cannot leave a server sending forever, and a restart ends every stream: clients send `Tstream` again from the last sequence number they hold. The cost is a timeout to tune, and a replica cannot take over a running stream. |
| Every request gets one reply | many frames for one request | Frames carry the request's id and a sequence number. `Tstop` names the stream by that id and is idempotent: stopping a stopped or unknown stream succeeds. |
| Reliable, ordered delivery | loss and reordering tolerated | **Datagrams** (UDP, later QUIC-style unreliable datagrams), with gaps reported in-band and a small reorder window at the receiver. Forward error correction instead of retransmission is optional, per stream. A reliable fallback over TCP for networks that block UDP is offered: the same frames, which then never have gaps, at the price of head-of-line blocking. |
| Replies go back to the requester | the same, at a high rate | A forged `Tstream` could aim a stream at a victim. Frames go only to the request's source address, and a server may require a **return-routability cookie** first (one extra round trip, as DTLS's HelloVerifyRequest): the server answers `Tstream` with a cookie sealed under its ticket key that names the address, and starts only when the request comes back with it. Over TCP the connection already proves the address. |
| Per-request sealing with the ticket's key | packets that decrypt on their own | Per-frame AEAD with the sequence number in the associated data and a random or sequence-derived nonce, as SRTP and DTLS do, and a replay window at the receiver instead of strict order. Tickets work unchanged. |
| Writes are conditional and versioned | a lossy write stream into a file | Frames are delivered to the file with sequence numbers and gaps marked (a reader of the file sees frame headers), never silently spliced into a byte stream. An ordinary static file refuses stream writes with `permission denied`; only files that are declared streams take them. Write streams need a ticket with both the write and the stream right. |
| Per-client keys; replies to one address | one packet for many receivers (multicast) | **Group keys** handed out through each member's ticket exchange and rotated when members leave (compare SRTP with MIKEY, or MLS). Any member can then forge frames for the others, so the source signs frames (expensive) or uses TESLA-style delayed key disclosure. Joining a group is what needs authorization, and the server sends only to groups it is configured to serve. Feedback from every receiver would flood the sender, so reports are sampled or aggregated (RTCP-style scaling), or there are none and FEC alone carries the stream. |

### Open questions

- **Request shape.** One `Tstream` with a direction byte is proposed
  above. A duplex stream (a call) would be two streams sharing a ticket;
  whether one request should open both is open.
- **What a stream read means for each kind of file**: a growing file
  (follow it, as `tail -f`), a device-like file that produces frames, and
  an ordinary static file (paced delivery at `rate`).
- **How it looks in the namespace.** A read of a stream file returns the
  next frame with a small header (sequence number, gap), as Plan 9 and
  Inferno audio devices return blocks and RTP carries headers. Whether the
  header is in band or in a side file (`ctl`) is open.
- **Flow and rate.** The receiver reports loss and rate (as RTCP does)
  and the sender adapts or drops. There is no head-of-line blocking on
  datagrams.
- **Who serves streams**: `lethed` streaming a host file in either
  direction, and a guest-side server (a stream as a `file2chan` file) that
  the kernel and the emu can send from or receive into.
- **Multicast underneath.** smoltcp supports IGMP; the emu's host network
  would need a group join call in `purg_platform::net`. QEMU user
  networking does not carry multicast; tap or socket networking does.
  Broadcast is limited to the local link.

## 12. Transport

**TCP only in the first cut.** The frames are self-contained, so the same
bytes work over other transports, but each has its own costs:

- **UDP** would make Lethe truly connectionless: one frame per datagram,
  retransmission by the client on a timeout (safe, since reads are
  idempotent). It is not in the first cut because: a 32 KiB read does not
  fit in one datagram, so `count` must be capped near the path MTU
  (about 1200 bytes of data after headers and the ticket) or replies
  fragment, which loses whole replies on lossy links; a small `Treq` can
  draw a much larger `Rread`, so a spoofed source address makes a server
  an amplifier unless it first checks return routability (a cookie round
  trip, as for streams); and the client needs retransmission timers and
  duplicate suppression that TCP gives for free. The device and the server
  are written around one frame in, one frame out, so adding UDP later is a
  transport, not a protocol change. The clear frames are datagram-ready in
  the same way, and without a ticket and a tag more data fits under the
  MTU; the amplification problem is the same, and without tickets the
  cookie round trip is the only thing that would limit it.
- **QEMU integration** (a virtio device or a vhost-user backend) is the
  long-term transport for the kernel, so no network stack is needed. It is
  later work.

## 13. Comparison

| | 9P2000 / Styx | NFSv2 | HTTP range GET | Lethe |
|---|---|---|---|---|
| server state | fids, open files, tags per connection | none (file handles are self-describing) | none | none |
| naming | walk from an attached fid | lookup per component, file handles | URL path | absolute path per request |
| directory paging | read on an open directory fid, offset | `READDIR` with a cookie | none | `readdir` with an opaque cookie |
| reads | offset and count on an open fid | offset and count on a handle | byte ranges | offset and count on a path |
| change detection | qid version (rarely used by clients) | attributes and mtime | `ETag`, `If-None-Match` | qid version, conditional read |
| security | auth fid, then a record layer per connection here | AUTH_UNIX (none, really) | TLS per connection | tickets, every request sealed; none with `-A` on both ends |
| a lost connection | loses every fid and mount | loses nothing | loses nothing | loses nothing |
| replicas | no | yes, with shared handles | yes | yes, with a shared ticket key |
| writes | yes | yes (stateless, with known consistency problems) | PUT, conditional | later, conditional |

Plan 9's `cfs` caches a 9P server's files on a local disk, checking qid
versions on open; `fossil` keeps a writable cache in front of a Venti
archive of content-addressed blocks. Lethe takes `cfs`'s idea, versions as
the cache key, and makes every read a validation. Its future whole-file
writes would take Venti's content addressing.

## 14. What the first cut implements

| piece | where | status |
|---|---|---|
| codec, tickets, key schedule, server engine, client state machine | `crates/purg-lethe` (`no_std` + `alloc`) | done |
| `Tauth`, `stat`, `read` (with `cond`), `readdir` | `purg-lethe` | done |
| ticket narrowing: rights and prefix | `purg-lethe`, `lethed`, `lethe` | done |
| the clear mode: `Tclear`, `Rclear`, `lethed -A`, `lethe -A`, `noauth` in `#λ` | `purg-lethe`, `lethed`, `#λ`, `lethe` | done |
| fuzz target and smoke test | `fuzz/fuzz_targets/lethe.rs`, `crates/purg-lethe/tests/fuzz_smoke.rs` | done |
| `lethed`, the host server | `crates/purg-emu/src/lethed.rs` (binary `src/bin/lethed.rs`), run with `cargo xtask lethed` | done |
| `#λ`, the client device | `crates/purg-core/src/devlethe.rs` | done |
| `lethe`, the attach command | `appl/cmd/lethe` | done |
| TCP | | done |
| tests | `purg-lethe` unit and engine tests; `crates/purg-emu/tests/lethe.rs` (both VMs); `root/lib/test/sh/lethe.sh`; the kernel's `lethed=PORT` check, sealed and clear | done |
| UDP, streaming, writes, `Tdigest`, server proofs, ticket key rotation | | later |
| boot root from Lethe | | later (below) |

### Where the pieces live

- `purg-lethe` is a shared crate, built for `aarch64-unknown-none`. It
  holds everything both ends need, so the kernel device and `lethed` run
  the same codec and the same checks. It depends on `purg-auth` (keys,
  certificates, the trust check), `purg-styx` (qids and stat entries) and
  the RustCrypto crates. XChaCha20-Poly1305 (`crates/purg-lethe/src/aead.rs`)
  is the RFC 8439 construction over `chacha20` 0.10's XChaCha20 and
  `poly1305`; `chacha20` 0.10 uses NEON on aarch64 targets that have it,
  the kernel's included, chosen at compile time (0.9 needed `--cfg
  chacha20_force_neon`). With the `ring` feature, which the emu and
  `lethed` turn on, the bulk is ring's ChaCha20-Poly1305 under the
  XChaCha20 subkey: HChaCha20 of the key and the nonce's first 16 bytes,
  then the nonce `0[4] || nonce[16..24]`. The bytes are the same either
  way; the tests check both backends against the RustCrypto
  `XChaCha20Poly1305`, sealing and opening, and against the draft's
  vectors.
- `lethed` is a second binary of `purg-emu`, not a crate of its own. It
  serves host files and needs exactly what `#U` already has in
  `crates/purg-emu/src/host/`: Inferno error strings for host errors,
  owner names, qids from inodes, names that are case-exact on macOS's
  case-insensitive volumes, and hidden entries. Keeping it next to `#U`
  means one host layer and the rule against `cfg(target_os)` outside
  `host/` holds without duplication. `cargo xtask lethed -- args` builds and
  runs it.
- `#λ` is a portable device in `purg-core`, behind the `lethe` feature
  (on in the emu, the probe and the kernel). It adds one hook to the
  device table, `Dev::devno`, so that each connection's files get their
  own `Chan::devno` and two trees never share a mount-table key. It dials through the platform's network
  (`Platform::net`), the stack `#I` uses, so it can redial on its own
  after a failure. Lambda for Lethe: Inferno and Plan 9 use Greek letters
  for some devices, and no Inferno or Plan 9 device uses this one (`#L`,
  the obvious letter, is the parallel port in both).

### `lethed`

```
lethed [-v] [-A] [-C alg] [-a addr] [-u owner] [-t ticketkey] [-e lifetime] [-w window]
       [-k keyfile ...] [-U userdir] [-s signers] [-r revoked] root
```

- `root`: the host directory served, read only.
- `-a`: the TCP address to listen on (default `127.0.0.1:5640`; port 0
  picks one). It prints `lethed: listening on <addr>` once ready.
- `-k`: a public key file (32 bytes) trusted for any user name, as a
  `* <key>` line of `/lib/keys/users` is for an export; repeatable. The
  tests pass the public test key, `lib/test/auth.key.pub`; nothing trusts
  it by default. `-U`: a directory of
  `<uname>/key.pub` files, each trusted for its user only, as an export
  trusts `usr/<uname>/key.pub`. `-s` and `-r`: a trusted signer list and a
  revocation list (`/lib/keys` formats); a revocation list that is there
  but does not parse fails closed. At least one of `-k`, `-U` and `-s` is
  required, unless `-A` is given: without any of them `lethed` does not
  start.
- `-A`: also answer clear requests, with no authentication and no
  encryption, as the user `none` (Section 17). `-A` alone serves in the
  clear only. `-A` with `-k`, `-U` or `-s` serves both: clients with a
  trusted key get tickets and sealed replies under their own names, and
  clear requests are answered as `none`.
- `-t`, `-e`, `-w`: the ticket key file, the ticket lifetime and the
  request window, in seconds.
- `-C x25519mlkem768`: issue tickets only from the hybrid exchange and
  take requests only on such tickets ("The post-quantum exchange");
  `-C x25519` names the default.
- `-u`: the name host files are owned by in stat entries and permission
  checks; by default each file's host owner, as `#U` shows it.
- `-v`: log each ticket issued and each request on standard error. Clear
  requests are logged for `none`.

**Names that escape the root.** Paths with `..` never reach the tree:
the codec refuses them (`file name syntax`). Symbolic links are followed
only when the result stays inside the root: `lethed` resolves the whole
path with the host's `canonicalize` and refuses anything outside the
canonical root as `file does not exist`, the same answer as for a name
that is not there, so a probe cannot tell a link out from a missing file.
Directory listings leave such links out. A link replaced between the check
and the open is a race `lethed` does not close; a tree whose links can be
changed by someone untrusted should not be served.

### The device and the command

```
lethe [-A] [-C alg] [-k keyfile] [-t ms] [-u user] [-abc] addr mtpt [path]
```

`lethe` resolves `addr` with `/net/cs`, finds a key as `mount` does (`-k`
names an authinfo; else `/usr/<user>/keyring/default`, `/usr/<user>/key`,
then the test key), and writes the connection to `#λ/clone`:

```
dial tcp!10.0.2.2!5640
user glenda
key <seed hex>
cert <pgcert1 line>       (with an authinfo)
path /lib                 (optional: the subtree, which is also the ticket's prefix)
cache 1000                (optional: the attribute lifetime in ms, from -t)
```

`lethe -C x25519mlkem768` adds `kx x25519mlkem768`: every ticket comes
from the hybrid exchange ("The post-quantum exchange"). It does not go
with `-A`.

`lethe -A` asks for the clear mode. It reads no key and writes

```
dial tcp!10.0.2.2!5640
noauth
path /lib                 (optional: the subtree)
```

`noauth` takes the place of `user`, `key` and `cert`; a write with
`noauth` and any of them is refused (`bad arg in system call`), and
`lethe -A` with `-k` or `-u` is a usage error. A connection keeps the mode
its settings gave it for as long as it lives. In the clear mode the
device sends `Tclear` and accepts only `Rclear` or the outer `Rerror`; in
the sealed mode it accepts only `Rauth`, `Rreq` and `Rerror`. So nothing a
server or someone in between sends can move it from one mode to the
other. The cache, the conditional reads, the pool, the redial and the
directory paging below are the same in both modes; only the ticket and
its renewal are missing in the clear.

The write dials and authenticates, so a refused key fails it with the
server's error; a server that does not answer fails it at once, without
the retries below. Reading `clone` then gives the connection's number `n`,
and `lethe` binds `#λn` on `mtpt` with the `-a`, `-b` or `-c` flags. The
device keeps the key to renew the ticket; a guest factotum cannot be used
in the first cut, since the device must be able to sign again later.

The device presents the tree as files. A walk is a `walk` of the names
it has left (the namespace hands a device all of them that it takes,
`Dev::walk_names`, the interface `#M` uses for a 9P `Twalk` of
several names), or a `stat` of the joined path when one name is left or
the server is older; a read is a `read` (conditional when
cached); a directory read pages through `readdir`. A stat right after a
walk is answered from the walk's entry while it is young. Connections are pooled: each request takes an idle
connection or dials a new one, and a connection that fails is dropped and
the request retried on a new one, a few times with a short pause, so a
server that restarts mid-read costs the reader a delay and no error. A
ticket that the server refuses is renewed and the request sent again.
The device dials through the kernel's own stack (`Platform::net`), not
through the namespace's `/net`, so an imported `/net` does not change
where it connects; the command translates names with `/net/cs` first.
Requests block the calling prog; there is no timeout yet, so a server that
accepts and never answers hangs its readers.

The data cache holds 32 KiB blocks (Lethe's largest read) by path and
version, at most 4 MiB a connection; the least recently used blocks go
first. The attribute cache (Section 5) holds at most 8,192 entries, and
drops those older than the lifetime when it is full. A directory listing
fills it, so `ls -l` of a listed directory stats each name without a
round trip. Directory pages are 32 KiB too.

A reader is sequential when it reads block 0 of a file whose length is
known, or a block whose predecessor is cached. Then the read of the block
goes out with reads of the next four, on one connection, and their
replies are matched by id: 160 KiB a round trip instead of 32. Readahead
stops at the file's known length, so a small file costs one read. It is
dropped, and the block read alone, if that connection fails.

`lethed` keeps three soft caches, each trusted for a second and lost
harmlessly in a restart: resolved paths (so a stat of a path costs one
`lstat` of its last element once its parent is known, and a link is
resolved in full and checked against the root), open files (each read
checks that the path still names the same file, by its qid, before using
the cached one), and the sorted names of directories by their qid, so a
large directory is read and sorted once, not once per page. A file
renamed over or a link changed is seen up to a second late by `lethed`;
the qid check means an open file is never read for another file's
version.

## 15. Boot root from Lethe (later)

The footprint work (#66) found that the compiled-in root is nine tenths of
the kernel image. Booting from Lethe would need:

- the network up before the root is mounted (it is, after `irq::init`);
- a key in the kernel before any file is read: from the bootargs (a seed
  in the clear on the command line is weak), from a device tree property,
  or from a small initrd holding only `/lib/keys` and the key;
- a minimal root (`#/` with `/bin/sh` and `lethe` at least, or the device
  attached directly by the kernel from a `letheroot=tcp!host!port`
  bootarg, which needs no guest);
- a cache, since every exec reads its module: the version check makes a
  RAM cache safe, and `Tdigest` would let it be keyed by content.

None of this changes the protocol.

## 16. Cost

Measured on an Apple Silicon host under Linux:

- The kernel's `.text` (`--profile kernel`) grows by 28 KiB, from 1,549
  to 1,577 KiB (1.8%), with `#λ` and `purg-lethe`; `.rodata` by 1 KiB.
  No crate is new: XChaCha20-Poly1305, HKDF, X25519 and Ed25519 were
  already linked for the mount's version 3 handshake. A kernel built
  without the `lethe` feature leaves all of it out.
- The changes of issue #138 (`Twalk`, the caches, readahead, the AEAD
  over `chacha20` 0.10) add 24 KiB of `.text` (1,737 to 1,761 KiB, 1.4%)
  and 3 KiB of `.rodata`; `chacha20` 0.10 brings `cipher` 0.5 and its
  helpers next to the 0.9 the record layer keeps.
- Test time: the hosted tests in `crates/purg-emu/tests/lethe.rs` took
  about 6 s for both VMs before the clear mode, most of it the
  server-restart and expiry checks, and its tests add a few seconds; the kernel's check adds a few seconds to each QEMU run (each
  run takes about 150 s under TCG).

## 17. The clear mode

Lethe can run with no authentication and no encryption. It is an option,
asked for on both ends, as `-A` is for a Styx mount and its export:

```
lethed -A root                    the server answers clear requests
lethe -A tcp!host!5640 /n/x       the client sends them
```

### Rules

- **The server.** Without `-A`, a clear request is refused with
  `authentication required` and nothing is read. With `-A` it is answered
  as `none` (Section 9). A server with neither `-A` nor a key to trust
  still does not start, and an engine in that state answers every `Tauth`
  with `server trusts no keys`.
- **`-A` alone** means clear only. Nobody can get a ticket; a `Tauth` is
  answered `authentication not required`, as an export with `-A` answers
  one.
- **`-A` with trust** (`-k`, `-U`, `-s`) means both. This follows the
  Styx export, where `-A` lets everyone in whatever keys are there, with
  one difference that comes from Lethe having no negotiation: a Styx
  mount that authenticates is told `authentication not required` and
  goes on in the clear, while a Lethe client that authenticates gets its
  ticket and stays sealed, under its own name and with its owner's bits.
  `-A` does not weaken the check on a `Tauth`: an untrusted key is still
  `authentication failed`. A user the server holds no key for (`-U`
  without that user, and no `-k` or `-s`) is told `authentication not
  required`.
- **Tickets that open are honoured** by any server holding their ticket
  key, `-A` or not, as before. A server gives none out unless it trusts
  a key, so this matters only for replicas that share a ticket key.
- **The client.** `lethe -A` sends clear requests and nothing else;
  `lethe` without it authenticates and seals and nothing else. Neither
  falls back. The two mismatches are errors of the attach:

  | client | server | error |
  |---|---|---|
  | `lethe -A` | `lethed` without `-A` | `authentication required` |
  | `lethe` | `lethed -A` alone | `authentication not required` |

### What it gives up

- **Confidentiality.** Names, data, directory listings and errors cross
  the wire as they are. Anyone on the path reads them.
- **Integrity.** Nothing protects a request or a reply. Someone in the
  middle can change the data read, the entries listed, the qid version
  (so a cache keeps or drops the wrong blocks) or the error, and can
  answer in the server's place.
- **Identity.** The server does not know who asks, so everyone is `none`
  and files not readable by others cannot be read at all. Tickets, with
  their rights and prefixes, do not exist in the clear.
- **Replay protection.** There is no time window. For reads this costs
  nothing: a replayed request gets the same public answer again. It is one
  reason writes, when they come, will not be offered in the clear without
  a separate decision.

### What still holds

- **Signed modules.** A module's signatures are inside it and cover its
  content digest (`docs/signing.md`), and the loader checks them whatever
  the module came over. Under the module policy `require`, a module
  changed on a clear link does not load. The default policy is `none`,
  which loads anything, so this protection has to be turned on. It is
  end-to-end integrity for signed modules only: unsigned files, directory
  listings and plain data have no such check.
- **The server's own rules.** Path syntax, the root-escape rules for
  symbolic links, permission bits, search on each directory and the
  read-only tree are enforced the same way.
- **The default.** A server not started with `-A` and a client not
  started with `-A` behave exactly as before the clear mode existed.

### What it is for

- **Trusted links**: a host and its own VM (QEMU user networking to
  `10.0.2.2`, later a virtio transport), a loopback, or a link that is
  already protected underneath.
- **Measurement**: comparing Lethe with and without its cryptography, and
  with Styx mounted `-A`.

It is not for a network someone else can reach. A boot root fetched in
the clear (Section 15) would need signed modules required for everything
it runs, and even then leaves listings and unsigned files open to change.
